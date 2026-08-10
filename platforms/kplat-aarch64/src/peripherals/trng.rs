// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ARM SMCCC TRNG 1.0 firmware service (DEN0098).

use core::sync::atomic::{AtomicBool, Ordering};

use super::smccc::{self, SmcccResult};

const TRNG_VERSION: u32 = 0x8000_0050;
const TRNG_RND64: u32 = 0x8000_0054;
const TRNG_MIN_VERSION: usize = 0x0001_0000;

/// DEN0098 allows up to three 64-bit words (192 bits) per `TRNG_RND64` call.
const MAX_BITS_PER_CALL: usize = 192;

const SMCCC_RET_SUCCESS: usize = smccc::SMCCC_RET_SUCCESS;
const SMCCC_RET_TRNG_INVALID_PARAMETER: isize = -2;
/// Firmware has no entropy available right now (DEN0098).
const SMCCC_RET_TRNG_NO_ENTROPY: isize = -3;

static TRNG_AVAILABLE: AtomicBool = AtomicBool::new(false);

/// Error returned while reading from the SMCCC TRNG firmware service.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrngReadError {
    /// [`init_trng`] did not find a usable TRNG service.
    Unavailable,
    /// Firmware reports that no entropy is available right now.
    NoEntropy,
    /// Firmware rejected the request size.
    InvalidParameter,
    /// Firmware returned an unexpected SMCCC error.
    Failed,
}

/// Probe the TRNG service through the discovered SMCCC conduit.
pub fn init_trng() {
    if let Some((major, minor)) = probe() {
        log::info!(
            "AArch64 SMCCC TRNG available ({}), version {major}.{minor}",
            smccc::standard_conduit().as_str()
        );
        TRNG_AVAILABLE.store(true, Ordering::Release);
    }
}

/// Returns whether [`init_trng`] found a usable TRNG service.
#[inline]
pub fn trng_available() -> bool {
    TRNG_AVAILABLE.load(Ordering::Acquire)
}

/// Fill `buf` with bytes from `TRNG_RND64`.
///
/// Returns the number of bytes written. A zero-length buffer succeeds without
/// calling firmware.
pub fn read_trng_random(buf: &mut [u8]) -> Result<usize, TrngReadError> {
    if buf.is_empty() {
        return Ok(0);
    }
    if !trng_available() {
        return Err(TrngReadError::Unavailable);
    }

    read_trng_random_with(buf, |func, bits| smccc::invoke(func, bits, 0, 0))
}

fn read_trng_random_with(
    buf: &mut [u8],
    mut invoke: impl FnMut(u32, usize) -> SmcccResult,
) -> Result<usize, TrngReadError> {
    let mut filled = 0;
    while filled < buf.len() {
        let remain = buf.len() - filled;
        let bits = (remain * 8).min(MAX_BITS_PER_CALL);
        // DEN0098: x1 = number of entropy bits requested (1..=192 for RND64).
        let result = invoke(TRNG_RND64, bits);

        match result.x0 as isize {
            v if v == SMCCC_RET_SUCCESS as isize => {
                let bytes = bits / 8;
                filled += copy_from_registers(&mut buf[filled..filled + bytes], &result);
            }
            SMCCC_RET_TRNG_NO_ENTROPY if filled > 0 => break,
            SMCCC_RET_TRNG_NO_ENTROPY => return Err(TrngReadError::NoEntropy),
            SMCCC_RET_TRNG_INVALID_PARAMETER if filled > 0 => break,
            SMCCC_RET_TRNG_INVALID_PARAMETER => return Err(TrngReadError::InvalidParameter),
            _ if filled > 0 => break,
            _ => return Err(TrngReadError::Failed),
        }
    }

    Ok(filled)
}

fn probe() -> Option<(u16, u16)> {
    let result = smccc::invoke(TRNG_VERSION, 0, 0, 0);
    if (result.x0 as isize) < 0 || result.x0 < TRNG_MIN_VERSION {
        return None;
    }

    let major = ((result.x0 >> 16) & 0xffff) as u16;
    let minor = (result.x0 & 0xffff) as u16;
    Some((major, minor))
}

/// Copy entropy from SMCCC return registers in DEN0098 / Linux order: x3, x2, x1.
fn copy_from_registers(buf: &mut [u8], result: &SmcccResult) -> usize {
    let words = [result.x3, result.x2, result.x1];
    let mut copied = 0;
    for word in words {
        if copied >= buf.len() {
            break;
        }
        let bytes = word.to_ne_bytes();
        let take = (buf.len() - copied).min(bytes.len());
        buf[copied..copied + take].copy_from_slice(&bytes[..take]);
        copied += take;
    }
    copied
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert, assert_eq, def_test};

    use super::*;

    fn success_result() -> SmcccResult {
        SmcccResult {
            x0: SMCCC_RET_SUCCESS,
            x1: 0x1111_2222_3333_4444,
            x2: 0x5555_6666_7777_8888,
            x3: 0x9999_aaaa_bbbb_cccc,
        }
    }

    #[def_test]
    fn test_read_trng_random_with_reports_no_entropy_before_data() {
        let mut buf = [0u8; 32];
        let err = read_trng_random_with(&mut buf, |_, _| SmcccResult {
            x0: SMCCC_RET_TRNG_NO_ENTROPY as usize,
            ..SmcccResult::default()
        })
        .err();
        assert_eq!(err, Some(TrngReadError::NoEntropy));
    }

    #[def_test]
    fn test_read_trng_random_with_reports_invalid_parameter_before_data() {
        let mut buf = [0u8; 32];
        let err = read_trng_random_with(&mut buf, |_, _| SmcccResult {
            x0: SMCCC_RET_TRNG_INVALID_PARAMETER as usize,
            ..SmcccResult::default()
        })
        .err();
        assert_eq!(err, Some(TrngReadError::InvalidParameter));
    }

    #[def_test]
    fn test_read_trng_random_with_reports_unknown_error_before_data() {
        let mut buf = [0u8; 32];
        let err = read_trng_random_with(&mut buf, |_, _| SmcccResult {
            x0: (-99isize) as usize,
            ..SmcccResult::default()
        })
        .err();
        assert_eq!(err, Some(TrngReadError::Failed));
    }

    #[def_test]
    fn test_read_trng_random_with_keeps_partial_bytes_on_no_entropy() {
        let mut calls = 0;
        let mut max_bits_seen = 0;
        let mut buf = [0u8; 32];
        let read = read_trng_random_with(&mut buf, |_, bits| {
            max_bits_seen = max_bits_seen.max(bits);
            calls += 1;
            if calls == 1 {
                success_result()
            } else {
                SmcccResult {
                    x0: SMCCC_RET_TRNG_NO_ENTROPY as usize,
                    ..SmcccResult::default()
                }
            }
        })
        .expect("partial data should be returned");
        assert_eq!(read, MAX_BITS_PER_CALL / 8);
        assert_eq!(calls, 2);
        assert!(max_bits_seen <= MAX_BITS_PER_CALL);
    }

    #[def_test]
    fn test_read_trng_random_with_keeps_partial_bytes_on_invalid_parameter() {
        let mut calls = 0;
        let mut buf = [0u8; 32];
        let read = read_trng_random_with(&mut buf, |_, _| {
            calls += 1;
            if calls == 1 {
                success_result()
            } else {
                SmcccResult {
                    x0: SMCCC_RET_TRNG_INVALID_PARAMETER as usize,
                    ..SmcccResult::default()
                }
            }
        })
        .expect("partial data should be returned");
        assert_eq!(read, MAX_BITS_PER_CALL / 8);
    }

    #[def_test]
    fn test_read_trng_random_with_keeps_partial_bytes_on_unknown_error() {
        let mut calls = 0;
        let mut buf = [0u8; 32];
        let read = read_trng_random_with(&mut buf, |_, _| {
            calls += 1;
            if calls == 1 {
                success_result()
            } else {
                SmcccResult {
                    x0: (-99isize) as usize,
                    ..SmcccResult::default()
                }
            }
        })
        .expect("partial data should be returned");
        assert_eq!(read, MAX_BITS_PER_CALL / 8);
    }
}

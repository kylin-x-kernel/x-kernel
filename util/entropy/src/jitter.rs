// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Software jitter entropy from timing variation (timer + interrupt noise).
//!
//! Each sample measures [`khal::time::now_ticks`] deltas across variable-length
//! busy waits. Interrupts, cache effects, and scheduling perturb the elapsed
//! time, producing low-rate entropy suitable as a fallback when HRNG is absent.

use alloc::vec::Vec;
use core::hint::spin_loop;

use khal::time::now_ticks;

const JITTER_SAMPLES: usize = 64;
const MIN_SPIN: u32 = 16;

/// Log jitter availability once during pool init.
pub(crate) fn init() {
    if is_available() {
        log::info!("entropy: software jitter source enabled");
    }
}

/// Jitter is available whenever the Kconfig option is enabled.
pub(crate) fn is_available() -> bool {
    kbuild_config::KFEAT_ENTROPY_JITTER
}

/// Collect timing jitter into a new buffer up to `len` bytes.
pub(crate) fn read(len: usize) -> Option<Vec<u8>> {
    if len == 0 || !is_available() {
        return None;
    }

    let mut buf = alloc::vec![0u8; len];
    let written = collect_into(&mut buf);
    if written == 0 {
        return None;
    }
    buf.truncate(written);
    Some(buf)
}

fn collect_into(out: &mut [u8]) -> usize {
    let mut state = seed_state();
    let mut out_idx = 0;

    for sample in 0..JITTER_SAMPLES {
        if out_idx >= out.len() {
            break;
        }

        let spin = MIN_SPIN.wrapping_add((state as u32) % 128);
        let start = now_ticks();
        for _ in 0..spin {
            spin_loop();
        }
        let end = now_ticks();
        let delta = end.wrapping_duration_since(start).as_raw();

        state ^= delta.rotate_left((sample as u32) & 31);
        state = state.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        state ^= (sample as u64).wrapping_mul(0xd134_2543_de82_ef95);

        for byte in delta.to_le_bytes().iter().chain(state.to_le_bytes().iter()) {
            if out_idx >= out.len() {
                break;
            }
            out[out_idx] ^= *byte;
            out_idx += 1;
        }
    }

    out_idx
}

fn seed_state() -> u64 {
    let ticks = now_ticks().as_raw();
    let stack_addr = &ticks as *const u64 as u64;
    ticks ^ stack_addr.rotate_left(17) ^ 0xa5a5_a5a5_a5a5_a5a5
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert, assert_eq, assert_ne, def_test};

    use super::*;

    #[def_test]
    fn test_jitter_availability_matches_kconfig() {
        assert_eq!(is_available(), kbuild_config::KFEAT_ENTROPY_JITTER);
    }

    #[def_test]
    fn test_jitter_read_zero_len() {
        assert!(read(0).is_none());
    }

    #[def_test]
    fn test_jitter_collect_nonzero() {
        if is_available() {
            let mut buf = [0u8; 32];
            let written = collect_into(&mut buf);
            assert_ne!(written, 0);
            assert_ne!(buf, [0u8; 32]);
        }
    }

    #[def_test]
    fn test_jitter_read_returns_requested_len() {
        if is_available() {
            let data = read(48).expect("jitter should produce samples");
            assert_eq!(data.len(), 48);
            assert!(data.iter().any(|&b| b != 0));
        } else {
            assert!(read(16).is_none());
        }
    }

    #[def_test]
    fn test_jitter_two_reads_differ() {
        if is_available() {
            let a = read(32).expect("first jitter read");
            let b = read(32).expect("second jitter read");
            assert_ne!(a.as_slice(), b.as_slice());
        }
    }
}

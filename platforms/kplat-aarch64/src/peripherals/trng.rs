// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ARM SMCCC TRNG 1.0 firmware service (DEN0098).

use core::sync::atomic::{AtomicBool, Ordering};

use super::smccc::{self, SmcccResult};

const TRNG_VERSION: u32 = 0x8000_0050;
const TRNG_RND64: u32 = 0x8000_0054;

/// DEN0098 allows up to three 64-bit words (192 bits) per `TRNG_RND64` call.
const MAX_BITS_PER_CALL: usize = 192;

const SMCCC_RET_SUCCESS: usize = smccc::SMCCC_RET_SUCCESS;
/// Firmware has no entropy available right now (DEN0098).
const SMCCC_RET_TRNG_NO_ENTROPY: isize = -3;

static TRNG_AVAILABLE: AtomicBool = AtomicBool::new(false);
static USE_HVC: AtomicBool = AtomicBool::new(false);

/// Probe HVC/SMC for the TRNG service and cache the working conduit.
pub fn init_trng() {
    // HVC first: QEMU virt and other EL1 guests route firmware calls through
    // HVC; SMC from EL1 is often undefined and must not panic during probe.
    if let Some((major, minor)) = probe(true) {
        USE_HVC.store(true, Ordering::Release);
        log::info!("AArch64 SMCCC TRNG available (hvc), version {major}.{minor}");
        TRNG_AVAILABLE.store(true, Ordering::Release);
        return;
    }

    if let Some((major, minor)) = probe(false) {
        log::info!("AArch64 SMCCC TRNG available (smc), version {major}.{minor}");
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
/// Returns the number of bytes written. Returns zero when TRNG is unavailable
/// or the firmware reports an error.
pub fn read_trng_random(buf: &mut [u8]) -> usize {
    if !trng_available() {
        return 0;
    }

    let mut filled = 0;
    while filled < buf.len() {
        let remain = buf.len() - filled;
        let bits = (remain * 8).min(MAX_BITS_PER_CALL);
        // DEN0098: x1 = number of entropy bits requested (1..=192 for RND64).
        let result = invoke(TRNG_RND64, bits, USE_HVC.load(Ordering::Acquire));

        match result.x0 as isize {
            v if v == SMCCC_RET_SUCCESS as isize => {
                let bytes = bits / 8;
                filled += copy_from_registers(&mut buf[filled..filled + bytes], &result);
            }
            SMCCC_RET_TRNG_NO_ENTROPY => break,
            _ => break,
        }
    }

    filled
}

fn probe(hvc: bool) -> Option<(u16, u16)> {
    // DEN0098: TRNG_VERSION returns major/minor in x0 on success, or a
    // negative SMCCC error (e.g. NOT_SUPPORTED) when absent.
    let result = invoke(TRNG_VERSION, 0, hvc);
    if (result.x0 as isize) < 0 || result.x0 == 0 {
        return None;
    }

    let major = ((result.x0 >> 16) & 0xffff) as u16;
    let minor = (result.x0 & 0xffff) as u16;
    Some((major, minor))
}

fn invoke(func: u32, arg0: usize, hvc: bool) -> SmcccResult {
    // VHE: kernel runs at EL2 so HVC traps to self; must use SMC.
    // Same convention as `psci::psci_call`. The `hvc` argument is ignored when
    // building with `feature = "vmm"`.
    if cfg!(feature = "vmm") {
        smccc::smc_call(func, arg0, 0, 0)
    } else if hvc {
        smccc::hvc_call(func, arg0, 0, 0)
    } else {
        smccc::smc_call(func, arg0, 0, 0)
    }
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

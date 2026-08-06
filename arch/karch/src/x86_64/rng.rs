// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! x86_64 CPU hardware RNG: RDSEED (preferred) and RDRAND (fallback).

use core::{
    arch::x86_64::{__cpuid, __cpuid_count, _rdrand64_step, _rdseed64_step, CpuidResult},
    sync::atomic::{AtomicBool, Ordering},
};

static RDSEED_AVAILABLE: AtomicBool = AtomicBool::new(false);
static RDRAND_AVAILABLE: AtomicBool = AtomicBool::new(false);

const MAX_WORD_RETRIES: u32 = 64;

/// Probe whether the current CPU implements RDSEED and/or RDRAND.
pub fn init_cpu_rng() {
    RDSEED_AVAILABLE.store(cpuid_has_rdseed(), Ordering::Release);
    RDRAND_AVAILABLE.store(cpuid_has_rdrand(), Ordering::Release);
}

/// Returns whether [`init_cpu_rng`] found a usable RDSEED or RDRAND.
#[inline]
pub fn cpu_rng_available() -> bool {
    RDSEED_AVAILABLE.load(Ordering::Acquire) || RDRAND_AVAILABLE.load(Ordering::Acquire)
}

/// Fill `buf` with bytes from RDSEED (preferred) or RDRAND.
///
/// Returns the number of bytes written. Returns zero when neither instruction
/// is available or every attempt failed.
pub fn read_cpu_random(buf: &mut [u8]) -> usize {
    if !cpu_rng_available() {
        return 0;
    }

    let prefer_rdseed = RDSEED_AVAILABLE.load(Ordering::Acquire);
    let allow_rdrand = RDRAND_AVAILABLE.load(Ordering::Acquire);
    let mut filled = 0;

    while filled < buf.len() {
        let Some(word) = read_word(prefer_rdseed, allow_rdrand) else {
            break;
        };
        let bytes = word.to_le_bytes();
        let take = (buf.len() - filled).min(bytes.len());
        buf[filled..filled + take].copy_from_slice(&bytes[..take]);
        filled += take;
    }

    filled
}

fn read_word(prefer_rdseed: bool, allow_rdrand: bool) -> Option<u64> {
    for attempt in 0..MAX_WORD_RETRIES {
        if prefer_rdseed && let Some(word) = try_rdseed64() {
            return Some(word);
        }
        // Prefer RDSEED; fall back to RDRAND periodically or when RDSEED is absent.
        if allow_rdrand
            && (!prefer_rdseed || attempt % 8 == 7)
            && let Some(word) = try_rdrand64()
        {
            return Some(word);
        }
    }
    None
}

fn try_rdseed64() -> Option<u64> {
    let mut value = 0u64;
    // SAFETY: `_rdseed64_step` is only called after CPUID confirmed RDSEED.
    // It writes a complete `u64` through a valid stack pointer when it
    // returns success (nonzero CF).
    let ok = unsafe { _rdseed64_step(&mut value) };
    (ok != 0).then_some(value)
}

fn try_rdrand64() -> Option<u64> {
    let mut value = 0u64;
    // SAFETY: `_rdrand64_step` is only called after CPUID confirmed RDRAND.
    // It writes a complete `u64` through a valid stack pointer when it
    // returns success (nonzero CF).
    let ok = unsafe { _rdrand64_step(&mut value) };
    (ok != 0).then_some(value)
}

fn cpuid_has_rdrand() -> bool {
    let CpuidResult { ecx, .. } = __cpuid(1);
    ecx & (1 << 30) != 0
}

fn cpuid_has_rdseed() -> bool {
    let max_leaf = __cpuid(0).eax;
    if max_leaf < 7 {
        return false;
    }
    let CpuidResult { ebx, .. } = __cpuid_count(7, 0);
    ebx & (1 << 18) != 0
}

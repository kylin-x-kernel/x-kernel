// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! AArch64 CPU hardware random number generator (RNDR / RNDRRS).

use core::sync::atomic::{AtomicBool, Ordering};

use aarch64_cpu::asm::random::ArmRng;

static CPU_RNG_AVAILABLE: AtomicBool = AtomicBool::new(false);

const MAX_WORD_RETRIES: u32 = 64;

/// Probe whether the current CPU implements the Armv8.5 RNG extension.
pub fn init_cpu_rng() {
    CPU_RNG_AVAILABLE.store(ArmRng::new().is_some(), Ordering::Release);
}

/// Returns whether [`init_cpu_rng`] found a usable RNDR implementation.
#[inline]
pub fn cpu_rng_available() -> bool {
    CPU_RNG_AVAILABLE.load(Ordering::Acquire)
}

/// Fill `buf` with bytes from RNDR/RNDRRS.
///
/// Returns the number of bytes written. Returns zero when RNG is unavailable
/// or every instruction attempt failed.
pub fn read_cpu_random(buf: &mut [u8]) -> usize {
    if !cpu_rng_available() {
        return 0;
    }

    let rng = ArmRng;
    let mut filled = 0;

    while filled < buf.len() {
        let Some(word) = read_word(&rng) else {
            break;
        };
        let bytes = word.to_le_bytes();
        let take = (buf.len() - filled).min(bytes.len());
        buf[filled..filled + take].copy_from_slice(&bytes[..take]);
        filled += take;
    }

    filled
}

fn read_word(rng: &ArmRng) -> Option<u64> {
    for attempt in 0..MAX_WORD_RETRIES {
        if let Some(word) = rng.rndr() {
            return Some(word);
        }
        if attempt % 8 == 7
            && let Some(word) = rng.rndrss()
        {
            return Some(word);
        }
    }
    None
}

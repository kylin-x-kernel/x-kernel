// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::sync::atomic::{AtomicUsize, Ordering};

static VERIFIED_CPUS: AtomicUsize = AtomicUsize::new(0);

fn read_register(msr: u32, offset: usize) -> u32 {
    if super::IS_X2APIC.load(Ordering::Relaxed) {
        // SAFETY: Boot selected x2APIC; callers supply only readable LAPIC MSRs.
        unsafe { x86_64::registers::model_specific::Msr::new(msr).read() as u32 }
    } else {
        let base = super::XAPIC_BASE.load(Ordering::Relaxed) as usize;
        // SAFETY: Boot mapped the LAPIC page before this check. Each offset
        // below is a readable, aligned register within that device mapping.
        unsafe { core::ptr::read_volatile((base + offset) as *const u32) }
    }
}

pub(super) fn verify_masked_preparation() {
    // Runs on each booting CPU before handle publication or timer activation.
    // Boot assertions deliberately abort if a caller regresses to enable().
    for (msr, offset) in [(0x832, 0x320), (0x835, 0x350), (0x836, 0x360), (0x837, 0x370)] {
        core::assert_ne!(read_register(msr, offset) & (1 << 16), 0);
    }
    core::assert_eq!(read_register(0x838, 0x380), 0);
    core::assert_eq!(read_register(0x832, 0x320) & (3 << 17), 0);
    core::assert_eq!(read_register(0x837, 0x370) & 0xff, u32::from(super::APIC_ERROR_VECTOR));
    VERIFIED_CPUS.fetch_add(1, Ordering::Relaxed);
}

#[unittest::def_test]
fn boot_checked_masked_preparation() {
    unittest::assert!(VERIFIED_CPUS.load(Ordering::Relaxed) > 0);
}

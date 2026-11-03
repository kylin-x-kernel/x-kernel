// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kalloc::UsageKind;
use unittest::def_test;

use super::GuestMem;

#[cfg(target_arch = "aarch64")]
type GuestTables = super::stage2::Stage2;
#[cfg(target_arch = "x86_64")]
type GuestTables = super::ept::Ept;
#[cfg(target_arch = "riscv64")]
type GuestTables = super::gstage::GStage;

#[def_test(serial)]
fn inactive_guest_tables_translate_and_release_all_page_storage() {
    const GUEST_BASE: u64 = 0x4000_0000;
    const HOST_BASE: u64 = 0x8000_0000;
    const SIZE: u64 = 4 * 1024 * 1024;
    let before = kalloc::global_allocator().usages().get(UsageKind::Global);
    for vmid in 1..=4 {
        // The table is never activated and no mapped address is accessed, so
        // translation geometry can be checked without hardware virtualization
        // or a guest-RAM allocation. Only real table-storage pages are owned.
        let tables = GuestTables::new(GUEST_BASE, SIZE, HOST_BASE, vmid).unwrap();
        assert_eq!(tables.gpa_to_hpa(GUEST_BASE), Some(HOST_BASE));
        assert_eq!(
            tables.gpa_to_hpa(GUEST_BASE + SIZE - 1),
            Some(HOST_BASE + SIZE - 1)
        );
        assert_eq!(tables.gpa_to_hpa(GUEST_BASE - 1), None);
        assert_eq!(tables.gpa_to_hpa(GUEST_BASE + SIZE), None);
        assert!(kalloc::global_allocator().usages().get(UsageKind::Global) > before);
        drop(tables);
        assert_eq!(
            kalloc::global_allocator().usages().get(UsageKind::Global),
            before
        );
    }
}

#[def_test(serial)]
fn overflowing_guest_range_does_not_allocate_table_storage() {
    let before = kalloc::global_allocator().usages().get(UsageKind::Global);
    assert!(GuestTables::new(u64::MAX - 4095, 8192, 0, 1).is_none());
    assert_eq!(
        kalloc::global_allocator().usages().get(UsageKind::Global),
        before
    );
}

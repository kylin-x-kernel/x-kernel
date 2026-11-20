// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::{
    GUEST_BASE, GuestMem, GuestTables, HOST_BASE, PAGE_BYTES, PageRef, RAM_BYTES, UsageKind, usage,
};

fn ensure_table(
    tables: &GuestTables,
    block: usize,
    allocate: &mut impl FnMut(usize, usize) -> Option<PageRef>,
) -> *mut u64 {
    #[cfg(target_arch = "aarch64")]
    {
        tables.ensure_l3_table_with_allocator(1, block, allocate)
    }
    #[cfg(target_arch = "riscv64")]
    {
        tables
            .ensure_l0_table_with_allocator(1, block, allocate)
            .unwrap_or(core::ptr::null_mut())
    }
}

fn block_descriptor(tables: &GuestTables, block: usize) -> u64 {
    assert!(block < 512);
    #[cfg(target_arch = "aarch64")]
    let root_pa = tables.l1_page.phys_addr().as_usize();
    #[cfg(target_arch = "riscv64")]
    let root_pa = tables.root_page.phys_addr().as_usize();
    let root = kaddr_layout::p2v(root_pa) as *const u64;
    // SAFETY: the fixture retains the initialized root and has no concurrent
    // walker or guest. Entry one is inside either supported root allocation.
    let parent = unsafe { root.wrapping_add(1).read() };
    #[cfg(target_arch = "aarch64")]
    let child_pa = parent & super::super::LPAE_ADDR_MASK;
    #[cfg(target_arch = "riscv64")]
    let child_pa = super::super::pte_pa(parent);
    let child = kaddr_layout::p2v(child_pa as usize) as *const u64;
    // SAFETY: construction linked this initialized child under root[1]; the
    // mapper owns it throughout the read and block < 512 was checked above.
    unsafe { child.wrapping_add(block).read() }
}

fn expected_leaf(block: u64, index: usize) -> u64 {
    #[cfg(target_arch = "aarch64")]
    {
        use super::super::{LPAE_ADDR_MASK, LPAE_TABLE, LPAE_VALID};
        let hpa = (block & LPAE_ADDR_MASK) + index as u64 * PAGE_BYTES as u64;
        hpa | (block & !LPAE_ADDR_MASK & !3) | LPAE_VALID | LPAE_TABLE
    }
    #[cfg(target_arch = "riscv64")]
    {
        use super::super::{PTE_LEAF_PERM, pte_addr, pte_pa};
        let hpa = pte_pa(block) + index as u64 * PAGE_BYTES as u64;
        pte_addr(hpa) | (block & PTE_LEAF_PERM)
    }
}

fn allocate_child(pages: usize, alignment_bytes: usize) -> Option<PageRef> {
    assert_eq!((pages, alignment_bytes), (1, PAGE_BYTES));
    Some(kalloc::alloc_zeroed_pages_exact_ref(pages, alignment_bytes, UsageKind::Global).unwrap())
}

#[def_test(serial)]
fn failed_split_preserves_block_and_reuse_needs_no_new_allocation() {
    let before = usage(UsageKind::Global);
    let tables = GuestTables::new(GUEST_BASE, RAM_BYTES, HOST_BASE, 1).unwrap();
    let initialized = usage(UsageKind::Global);
    let original = block_descriptor(&tables, 0);
    let mut calls = 0;
    let mut reject = |pages, alignment_bytes| {
        assert_eq!((pages, alignment_bytes), (1, PAGE_BYTES));
        calls += 1;
        None
    };
    assert!(ensure_table(&tables, 0, &mut reject).is_null());
    assert_eq!(block_descriptor(&tables, 0), original);
    assert_eq!(tables.gpa_to_hpa(GUEST_BASE + 4095), Some(HOST_BASE + 4095));
    assert_eq!(usage(UsageKind::Global), initialized);

    let child = ensure_table(&tables, 0, &mut allocate_child);
    assert!(!child.is_null());
    assert_eq!(usage(UsageKind::Global), initialized + PAGE_BYTES);
    for index in [0, 1, 511] {
        // SAFETY: child is a live split table owned by this inactive mapper;
        // these indices are in its 512 initialized entries, without concurrent access.
        let entry = unsafe { child.wrapping_add(index).read() };
        assert_eq!(entry, expected_leaf(original, index));
    }
    assert_eq!(ensure_table(&tables, 0, &mut reject), child);
    // A second block's failure must leave the first successful prefix owned.
    let next = block_descriptor(&tables, 1);
    assert!(ensure_table(&tables, 1, &mut reject).is_null());
    assert_eq!(block_descriptor(&tables, 1), next);
    assert_eq!(usage(UsageKind::Global), initialized + PAGE_BYTES);
    assert_eq!(calls, 2);
    // Drop must follow the surviving split-child link after the later failure.
    drop(tables);
    assert_eq!(usage(UsageKind::Global), before);
}

#[def_test(serial)]
fn empty_table_failure_is_unpublished_and_final_unmap_releases_child() {
    let before = usage(UsageKind::Global);
    let tables = GuestTables::new(GUEST_BASE, RAM_BYTES, HOST_BASE, 1).unwrap();
    let initialized = usage(UsageKind::Global);
    assert_eq!(block_descriptor(&tables, 2), 0);
    let mut calls = 0;
    let mut reject = |pages, alignment_bytes| {
        assert_eq!((pages, alignment_bytes), (1, PAGE_BYTES));
        calls += 1;
        None
    };
    assert!(ensure_table(&tables, 2, &mut reject).is_null());
    assert_eq!(block_descriptor(&tables, 2), 0);
    assert_eq!(usage(UsageKind::Global), initialized);
    let child = ensure_table(&tables, 2, &mut allocate_child);
    assert!(!child.is_null());
    for index in 0..512 {
        // SAFETY: the fixture exclusively retains this initialized zeroed child
        // through the inactive mapper; index remains within its 512 entries.
        assert_eq!(unsafe { child.wrapping_add(index).read() }, 0);
    }
    assert_eq!(ensure_table(&tables, 2, &mut reject), child);
    assert_eq!(calls, 1);
    assert_eq!(usage(UsageKind::Global), initialized + PAGE_BYTES);
    assert!(tables.map_region(
        GUEST_BASE + RAM_BYTES,
        HOST_BASE + RAM_BYTES,
        PAGE_BYTES as u64,
        crate::mm::GuestPerm::RamRw,
    ));
    // Mapping reused the prepared table; removing its last leaf must release it.
    assert_eq!(usage(UsageKind::Global), initialized + PAGE_BYTES);
    assert!(tables.unmap_range(GUEST_BASE + RAM_BYTES, PAGE_BYTES as u64));
    assert_eq!(block_descriptor(&tables, 2), 0);
    assert_eq!(usage(UsageKind::Global), initialized);
    drop(tables);
    assert_eq!(usage(UsageKind::Global), before);
}

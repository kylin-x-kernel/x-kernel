// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, vec::Vec};
use core::ptr::NonNull;

use kalloc::{GlobalAllocator, GlobalPage, UsageKind};
use memaddr::{PAGE_SIZE_4K as PAGE, PhysAddr, PhysAddrRange};
use unittest::def_test;

#[def_test]
fn prepared_inventory_works_with_existing_allocator_interfaces() {
    let storage = GlobalPage::alloc_contiguous(256, PAGE).unwrap();
    let va = storage.start_va();
    let pa = khal::mem::v2p(va);
    let mut free = [
        PhysAddrRange::from_start_size(pa, 64 * PAGE),
        PhysAddrRange::from_start_size(pa + 128 * PAGE, 64 * PAGE),
    ];
    // SAFETY: GlobalPage exclusively owns both disjoint mapped ranges. It is
    // declared first and outlives the directory, allocator and allocations.
    let table = unsafe {
        page_meta::init(&mut free, |backing| {
            NonNull::new(khal::mem::p2v(backing.start).as_mut_ptr())
                .ok_or(page_meta::PageMetaError::InvalidRange)
        })
    }
    .unwrap();
    let allocator = Box::new(GlobalAllocator::new());
    let first = free.as_slice()[0];
    allocator.init_page_allocator(khal::mem::p2v(first.start).as_usize(), first.size());
    for r in &free.as_slice()[1..] {
        if !r.is_empty() {
            allocator
                .add_memory(khal::mem::p2v(r.start).as_usize(), r.size())
                .unwrap();
        }
    }
    // Exercise inventory handoff using eight-page blocks: this fixture owns
    // a private allocator, while the PCP caches belong to the global allocator.
    const BLOCK_PAGES: usize = 8;
    let mut allocated = Vec::new();
    while let Ok(addr) = allocator.alloc_pages(BLOCK_PAGES, PAGE, UsageKind::Global) {
        let paddr = PhysAddr::from_usize(pa.as_usize() + (addr - va.as_usize()));
        for pfn in paddr.as_usize() / PAGE..paddr.as_usize() / PAGE + BLOCK_PAGES {
            assert!(!table.lookup(pfn).unwrap().is_permanent());
        }
        assert!(
            free.iter().any(
                |r| r.contains_range(PhysAddrRange::from_start_size(paddr, BLOCK_PAGES * PAGE))
            )
        );
        assert!(!allocated.contains(&addr));
        allocated.push(addr);
    }
    assert!(!allocated.is_empty());
    let exhausted = allocator.used_pages();
    for addr in allocated {
        allocator.dealloc_pages(addr, BLOCK_PAGES, UsageKind::Global);
    }
    assert!(allocator.used_pages() < exhausted);
}

#[def_test]
fn boot_publishes_page_metadata_independently_of_allocator_instances() {
    let page = GlobalPage::alloc_contiguous(1, PAGE).unwrap();
    let pa = khal::mem::v2p(page.start_va());
    let meta = page_meta::lookup(pa.as_usize() / PAGE).unwrap();
    assert!(!meta.is_permanent());
    let identity = meta as *const _;
    drop(page);
    assert_eq!(
        page_meta::lookup(pa.as_usize() / PAGE).unwrap() as *const _,
        identity
    );
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec::Vec;
use core::ptr::NonNull;
use kalloc_types::KBox;

use kalloc::{GlobalAllocator, UsageKind, alloc_page_ref, alloc_pages_exact_ref};
use memaddr::{PAGE_SIZE_4K as PAGE, PhysAddr, PhysAddrRange};
use unittest::def_test;

#[def_test]
fn prepared_inventory_works_with_existing_allocator_interfaces() {
    let storage = alloc_pages_exact_ref(256, PAGE, UsageKind::Global).unwrap();
    let pa = storage.phys_addr();
    let va = khal::mem::p2v(pa);
    let mut free = [
        PhysAddrRange::from_start_size(pa, 64 * PAGE),
        PhysAddrRange::from_start_size(pa + 128 * PAGE, 64 * PAGE),
    ];
    // SAFETY: this unpublished PageRef retains both exclusive mapped ranges.
    // It is declared first and outlives the table, allocator and allocations.
    let table = unsafe {
        page_meta::init(&mut free, |backing| {
            NonNull::new(khal::mem::p2v(backing.start).as_mut_ptr())
                .ok_or(page_meta::PageMetaError::InvalidRange)
        })
    }
    .unwrap();
    let allocator = KBox::new_nowait(GlobalAllocator::new()).unwrap();
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
    let page = alloc_page_ref(UsageKind::Global).unwrap();
    let pa = page.phys_addr();
    let meta = page_meta::lookup(pa.as_usize() / PAGE).unwrap();
    assert!(!meta.is_permanent());
    let identity = meta as *const _;
    drop(page);
    assert_eq!(
        page_meta::lookup(pa.as_usize() / PAGE).unwrap() as *const _,
        identity
    );
}

#[def_test(serial)]
fn shared_exact_and_compound_ranges_survive_concurrent_clone_and_drop() {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    const WORKERS: usize = 4;
    for compound in [false, true] {
        let before = kalloc::global_allocator().usages().get(UsageKind::Global);
        let owner = if compound {
            kalloc::alloc_compound_pages_ref(8, PAGE, UsageKind::Global).unwrap()
        } else {
            alloc_pages_exact_ref(8, PAGE, UsageKind::Global).unwrap()
        };
        let ready = Arc::new(AtomicUsize::new(0));
        let start = Arc::new(AtomicBool::new(false));
        let mut workers = Vec::new();
        for _ in 0..WORKERS {
            let pages = owner.try_clone_range(2, 3).unwrap();
            let ready = ready.clone();
            let start = start.clone();
            workers.push(ktask::spawn(move || {
                ready.fetch_add(1, Ordering::Release);
                while !start.load(Ordering::Acquire) {
                    ktask::yield_now();
                }
                for _ in 0..256 {
                    let copy = pages.try_clone_ref().unwrap();
                    let last = copy.try_clone_page(2).unwrap();
                    assert_eq!(last.phys_addr(), pages.phys_addr() + 2 * PAGE);
                    drop(copy);
                    drop(last);
                }
            }));
        }
        while ready.load(Ordering::Acquire) != WORKERS {
            ktask::yield_now();
        }
        // Every worker has acknowledged its hold; the initial complete owner
        // can end before any of those holds are cloned and released in parallel.
        drop(owner);
        start.store(true, Ordering::Release);
        for worker in workers {
            assert_eq!(worker.join(), 0);
        }
        assert_eq!(
            kalloc::global_allocator().usages().get(UsageKind::Global),
            before
        );
    }
}

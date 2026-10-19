// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc_engine::BaseAllocator;
use unittest::def_test;

use super::*;

#[def_test]
fn test_per_cpu_pages_new_empty() {
    let cache = PerCpuPages::<1, 12>::new();
    assert_eq!(cache.len(), 0);
}

#[def_test]
fn test_per_cpu_pages_push_pop() {
    let mut cache = PerCpuPages::<2, 6>::new();
    assert!(cache.try_push(0x2000).is_ok());
    assert_eq!(cache.len(), 1);
    assert_eq!(cache.try_pop(), Some(0x2000));
    assert_eq!(cache.len(), 0);
}

#[def_test]
fn test_per_cpu_pages_pop_empty_returns_none() {
    let mut cache = PerCpuPages::<1, 12>::new();
    assert_eq!(cache.try_pop(), None);
}

#[def_test]
fn test_per_cpu_pages_push_full_returns_err() {
    let mut cache = PerCpuPages::<4, 6>::new();
    for i in 0..6 {
        assert!(cache.try_push(0x1000 * (i + 1)).is_ok());
    }
    assert_eq!(cache.len(), 6);
    assert_eq!(cache.try_push(0xFFFF_0000), Err(0xFFFF_0000));
}

#[def_test]
fn test_per_cpu_pages_lifo_order() {
    let mut cache = PerCpuPages::<1, 12>::new();
    cache.try_push(0x1000).unwrap();
    cache.try_push(0x2000).unwrap();
    cache.try_push(0x3000).unwrap();
    assert_eq!(cache.try_pop(), Some(0x3000));
    assert_eq!(cache.try_pop(), Some(0x2000));
    assert_eq!(cache.try_pop(), Some(0x1000));
}

#[def_test]
fn test_constants_reasonable() {
    assert!(C1 >= 4);
    assert!(C2 >= 2);
    assert!(C3 >= 2);
    assert!(C4 >= 2);
}

#[def_test]
fn test_refill_fills_cache() {
    let mut palloc = BuddyPageAllocator::<PAGE_SIZE_4K>::new();
    let heap = alloc::vec![0u8; 64 * PAGE_SIZE_4K];
    let base = heap.as_ptr() as usize;
    palloc.add_region(base, heap.len()).unwrap();
    core::mem::forget(heap);

    let mut cache = PerCpuPages::<1, 12>::new();
    let addr = cache.refill(&mut palloc).unwrap();
    assert!((base..base + 64 * PAGE_SIZE_4K).contains(&addr));
    assert_eq!(cache.len(), 7); // COUNT * 2/3 blocks, minus the one returned
    assert_eq!(palloc.used_pages(), 8);
}

#[def_test]
fn test_refill_partial_when_buddy_short() {
    let mut palloc = BuddyPageAllocator::<PAGE_SIZE_4K>::new();
    let heap = alloc::vec![0u8; 3 * PAGE_SIZE_4K];
    let base = heap.as_ptr() as usize;
    palloc.add_region(base, heap.len()).unwrap();
    core::mem::forget(heap);

    let mut cache = PerCpuPages::<1, 12>::new();
    let addr = cache.refill(&mut palloc).unwrap();
    assert!((base..base + 3 * PAGE_SIZE_4K).contains(&addr));
    assert_eq!(cache.len(), 2); // 3 of 8 blocks obtained, 1 returned
    assert_eq!(palloc.used_pages(), 3);
}

#[def_test]
fn test_refill_returns_rounding_slack() {
    // NR_PAGES=3 is not a power of two: the buddy hands out an order-2
    // (4-page) chunk per block. The extra page must be returned so a
    // cached block occupies exactly 3 pages and drain's split_to_chunks
    // bookkeeping stays balanced.
    let mut palloc = BuddyPageAllocator::<PAGE_SIZE_4K>::new();
    let heap = alloc::vec![0u8; 64 * PAGE_SIZE_4K];
    let base = heap.as_ptr() as usize;
    palloc.add_region(base, heap.len()).unwrap();
    core::mem::forget(heap);

    let mut cache = PerCpuPages::<3, 6>::new();
    let addr = cache.refill(&mut palloc).unwrap();
    assert!((base..base + 64 * PAGE_SIZE_4K).contains(&addr));
    // nr_to_alloc = 4 blocks × 3 pages; 16 pages would mean the
    // rounding slack leaked.
    assert_eq!(palloc.used_pages(), 12);
    assert_eq!(cache.len(), 3);
}

#[def_test]
fn test_refill_fails_when_nothing_available() {
    let mut palloc = BuddyPageAllocator::<PAGE_SIZE_4K>::new();
    let heap = alloc::vec![0u8; PAGE_SIZE_4K];
    let base = heap.as_ptr() as usize;
    palloc.add_region(base, heap.len()).unwrap();
    core::mem::forget(heap);
    let taken = palloc.allocate_pages(1, PAGE_SIZE_4K).unwrap();

    let mut cache = PerCpuPages::<1, 12>::new();
    assert!(cache.refill(&mut palloc).is_err());
    assert_eq!(cache.len(), 0);
    palloc.deallocate_pages(taken, 1);
}

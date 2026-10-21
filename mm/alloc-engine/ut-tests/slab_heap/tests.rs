// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::alloc::Layout;

use unittest::def_test;

use super::*;

fn make_heap() -> SlabHeap {
    let mem = alloc::vec![0u8; 128 * PAGE_SIZE];
    let base = mem.as_ptr() as usize;
    let size = mem.len();
    core::mem::forget(mem);
    // SAFETY: the Vec's backing memory is leaked and remains valid.
    unsafe { SlabHeap::new(base, size) }
}

#[def_test]
fn test_alloc_dealloc_small() {
    let mut heap = make_heap();
    let layout = Layout::from_size_align(64, 8).unwrap();
    let a1 = heap.allocate(layout).unwrap();
    let a2 = heap.allocate(layout).unwrap();
    assert_ne!(a1, a2);
    // SAFETY: a1 was allocated above with this layout.
    unsafe { heap.deallocate(a1, layout) };
    // SAFETY: a2 was allocated above with this layout.
    unsafe { heap.deallocate(a2, layout) };
}

#[def_test]
fn test_reuse_freed_block() {
    let mut heap = make_heap();
    let layout = Layout::from_size_align(128, 8).unwrap();
    let a1 = heap.allocate(layout).unwrap();
    // SAFETY: a1 was allocated above with this layout.
    unsafe { heap.deallocate(a1, layout) };
    let a2 = heap.allocate(layout).unwrap();
    assert_eq!(a1, a2); // Should reuse the freed block.
}

#[def_test]
fn test_large_alloc() {
    let mut heap = make_heap();
    let layout = Layout::from_size_align(8192, 8).unwrap();
    let addr = heap.allocate(layout).unwrap();
    assert!(addr.is_multiple_of(PAGE_SIZE));
    // SAFETY: addr was allocated above with this layout.
    unsafe { heap.deallocate(addr, layout) };
}

#[def_test]
fn test_many_allocs() {
    let mut heap = make_heap();
    let mut addrs = alloc::vec::Vec::new();
    for _ in 0..100 {
        let layout = Layout::from_size_align(64, 8).unwrap();
        addrs.push(heap.allocate(layout).unwrap());
    }
    for &addr in addrs.iter() {
        let layout = Layout::from_size_align(64, 8).unwrap();
        // SAFETY: each addr was allocated above with this layout.
        unsafe { heap.deallocate(addr, layout) };
    }
}

#[def_test]
fn test_mixed_sizes() {
    let mut heap = make_heap();
    let l64 = Layout::from_size_align(64, 8).unwrap();
    let l256 = Layout::from_size_align(256, 8).unwrap();
    let l1024 = Layout::from_size_align(1024, 8).unwrap();

    let a64 = heap.allocate(l64).unwrap();
    let a256 = heap.allocate(l256).unwrap();
    let a1024 = heap.allocate(l1024).unwrap();

    // SAFETY: a256 was allocated above with l256.
    unsafe { heap.deallocate(a256, l256) };
    // SAFETY: a64 was allocated above with l64.
    unsafe { heap.deallocate(a64, l64) };
    // SAFETY: a1024 was allocated above with l1024.
    unsafe { heap.deallocate(a1024, l1024) };

    // Should be able to re-allocate.
    let _ = heap.allocate(l1024).unwrap();
}

#[def_test]
fn test_stats() {
    let heap = make_heap();
    assert!(heap.total_bytes() > 0);
    assert!(heap.available_bytes() > 0);
}

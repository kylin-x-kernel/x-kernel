// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::ptr::NonNull;

use unittest::def_test;

use super::*;

const PAGE: usize = 4096;

struct TestHeap {
    ptr: NonNull<u8>,
    layout: core::alloc::Layout,
}

impl TestHeap {
    fn new(pages: usize) -> Self {
        let layout = core::alloc::Layout::from_size_align(
            pages * PAGE,
            pages.next_power_of_two() * PAGE,
        )
        .unwrap();
        // SAFETY: layout has nonzero size and power-of-two alignment. The
        // returned storage remains owned by this fixture until Drop.
        let ptr = NonNull::new(unsafe { alloc::alloc::alloc_zeroed(layout) }).unwrap();
        Self { ptr, layout }
    }

    fn base(&self) -> usize {
        self.ptr.as_ptr() as usize
    }

    fn size(&self) -> usize {
        self.layout.size()
    }
}

impl Drop for TestHeap {
    fn drop(&mut self) {
        // SAFETY: Tests discard the nested buddy and its allocations before
        // dropping this unique backing allocation with its original layout.
        unsafe { alloc::alloc::dealloc(self.ptr.as_ptr(), self.layout) };
    }
}

fn make_alloc() -> (TestHeap, BuddyAllocator<PAGE>) {
    let heap = TestHeap::new(256);
    let mut buddy = BuddyAllocator::new();
    buddy.init_region(heap.base(), heap.size()).unwrap();
    (heap, buddy)
}

#[def_test]
fn test_init_and_stats() {
    let (_heap, b) = make_alloc();
    assert_eq!(b.total_pages(), 256);
    assert_eq!(b.free_pages(), 256);
    assert_eq!(b.used_pages(), 0);
}

#[def_test]
fn test_alloc_one_page() {
    let (_heap, mut b) = make_alloc();
    let addr = b.allocate_pages(1, PAGE).unwrap();
    assert!(addr.is_multiple_of(PAGE));
    assert_eq!(b.used_pages(), 1);
    assert_eq!(b.free_pages(), 255);
}

#[def_test]
fn test_alloc_dealloc_one_page() {
    let (_heap, mut b) = make_alloc();
    let addr = b.allocate_pages(1, PAGE).unwrap();
    b.deallocate_pages(addr, 1);
    assert_eq!(b.free_pages(), 256);
    assert_eq!(b.used_pages(), 0);
}

#[def_test]
fn test_alloc_multi_page() {
    let (_heap, mut b) = make_alloc();
    let addr = b.allocate_pages(4, PAGE).unwrap(); // order 2
    assert!(addr.is_multiple_of(PAGE));
    assert_eq!(b.used_pages(), 4);
    b.deallocate_pages(addr, 4);
    assert_eq!(b.free_pages(), 256);
}

#[def_test]
fn test_alloc_exhaust_then_free() {
    let (_heap, mut b) = make_alloc();
    let mut addrs = alloc::vec::Vec::new();
    // Allocate 256 single pages.
    for _ in 0..256 {
        addrs.push(b.allocate_pages(1, PAGE).unwrap());
    }
    assert_eq!(b.free_pages(), 0);
    assert!(b.allocate_pages(1, PAGE).is_err());

    for addr in addrs {
        b.deallocate_pages(addr, 1);
    }
    assert_eq!(b.free_pages(), 256);
}

#[def_test]
fn test_merge_on_dealloc() {
    let (_heap, mut b) = make_alloc();
    // Allocate 8 single pages (order 0), then free them — they should
    // merge into larger blocks.
    let mut addrs = alloc::vec::Vec::new();
    for _ in 0..8 {
        addrs.push(b.allocate_pages(1, PAGE).unwrap());
    }
    for addr in addrs {
        b.deallocate_pages(addr, 1);
    }
    // After merging, we should be able to alloc order-3 (8 pages).
    let big = b.allocate_pages(8, PAGE).unwrap();
    b.deallocate_pages(big, 8);
    assert_eq!(b.free_pages(), 256);
}

#[def_test]
fn test_alloc_large_order() {
    let (_heap, mut b) = make_alloc();
    let addr = b.allocate_pages(64, PAGE).unwrap(); // order 6
    assert_eq!(b.used_pages(), 64);
    b.deallocate_pages(addr, 64);
    assert_eq!(b.free_pages(), 256);
}

#[def_test]
fn test_add_region() {
    let heap1 = TestHeap::new(32);
    let heap2 = TestHeap::new(64);
    let mut b = BuddyAllocator::<PAGE>::new();
    b.init_region(heap1.base(), heap1.size()).unwrap();
    b.add_region(heap2.base(), heap2.size()).unwrap();
    assert_eq!(b.total_pages(), 96);
    assert_eq!(b.free_pages(), 96);
}

#[def_test]
fn test_overlap_rejected() {
    let heap = TestHeap::new(64);
    let mut b = BuddyAllocator::<PAGE>::new();
    b.init_region(heap.base(), heap.size()).unwrap();
    assert_eq!(
        b.add_region(heap.base(), 32 * PAGE),
        Err(AllocError::MemoryOverlap)
    );
}

#[def_test]
fn test_free_permutations_preserve_live_pages_and_fully_coalesce() {
    for pages in [256, 1024] {
        let heap = TestHeap::new(pages);
        for permutation in 0..3 {
            let mut b = BuddyAllocator::<PAGE>::new();
            b.init_region(heap.base(), heap.size()).unwrap();
            let total = b.total_pages();
            let mut addrs = alloc::vec::Vec::new();
            for _ in 0..total {
                let addr = b.allocate_pages(1, PAGE).unwrap();
                // SAFETY: The allocation is exclusive and one page long.
                // Arbitrary payload must never be read as free-list links.
                unsafe { core::ptr::write_bytes(addr as *mut u8, 0xa5, PAGE) };
                addrs.push(addr);
            }
            addrs.sort_unstable();
            assert!(!addrs.windows(2).any(|pair| pair[0] == pair[1]));
            let heap_start = addrs[0];
            match permutation {
                1 => addrs.reverse(),
                2 => {
                    let mut seed = 0x12345678u64;
                    for i in (1..addrs.len()).rev() {
                        seed ^= seed << 13;
                        seed ^= seed >> 7;
                        seed ^= seed << 17;
                        addrs.swap(i, seed as usize % (i + 1));
                    }
                }
                _ => {}
            }
            for (index, addr) in addrs.into_iter().enumerate() {
                // SAFETY: This page has not yet been freed. Other frees must
                // neither merge it nor overwrite its live payload.
                let first = unsafe { *(addr as *const u8) };
                // SAFETY: This is the last initialized byte of the same live
                // page; the allocation has not yet been returned to b.
                let last = unsafe { *((addr + PAGE - 1) as *const u8) };
                assert_eq!(first, 0xa5);
                assert_eq!(last, 0xa5);
                b.deallocate_pages(addr, 1);
                assert_eq!(b.free_pages(), index + 1);
            }
            // Recover every maximal chunk, not just the free-page counter.
            let chunks: alloc::vec::Vec<_> =
                split_to_chunks::<PAGE>(heap_start, total * PAGE).collect();
            for &(addr, order) in &chunks {
                assert_eq!(b.allocate_pages_at(addr, 1 << order, PAGE), Ok(addr));
            }
            assert_eq!(b.free_pages(), 0);
            for (addr, order) in chunks {
                b.deallocate_pages(addr, 1 << order);
            }
            assert_eq!(b.free_pages(), total);
        }
    }
}

#[def_test]
fn test_allocated_buddy_cannot_forge_free_membership() {
    let (_heap, mut b) = make_alloc();
    let live = b.allocate_pages(1, PAGE).unwrap();
    let released = b.allocate_pages(1, PAGE).unwrap();
    assert_eq!(live ^ PAGE, released);
    // SAFETY: This still-allocated page belongs exclusively to the test.
    // Even a well-formed header with a matching order is caller payload,
    // not evidence that the page belongs to an allocator free list.
    unsafe {
        (live as *mut FreeNode).write(FreeNode {
            link: LinkedListAtomicLink::new(),
            order: 0,
        });
    }
    b.deallocate_pages(released, 1);
    assert_eq!(b.free_pages(), 255);
    assert!(b.allocate_pages_at(live, 2, PAGE).is_err());
    b.deallocate_pages(live, 1);
    assert_eq!(b.allocate_pages_at(live, 2, PAGE), Ok(live));
    b.deallocate_pages(live, 2);
    assert_eq!(b.free_pages(), 256);
}

#[def_test]
fn test_bitmap_storage_is_reserved_and_overlap_checked() {
    let heap = TestHeap::new(1024);
    let mut b = BuddyAllocator::<PAGE>::new();
    b.init_region(heap.base(), heap.size()).unwrap();
    assert_eq!(b.total_pages(), 1023);
    assert_eq!(b.free_pages(), 1023);
    assert!(b.allocate_pages_at(heap.base(), 1, PAGE).is_err());
    assert_eq!(
        b.add_region(heap.base(), PAGE),
        Err(AllocError::MemoryOverlap)
    );
    let mut addrs = alloc::vec::Vec::new();
    while let Ok(addr) = b.allocate_pages(1, PAGE) {
        assert!(addr >= heap.base() + PAGE);
        addrs.push(addr);
    }
    assert_eq!(addrs.len(), 1023);
    for addr in addrs {
        b.deallocate_pages(addr, 1);
    }
    b.reset();
    b.add_region(heap.base(), heap.size()).unwrap();
    assert_eq!(b.free_pages(), 1023);
}

#[def_test]
fn test_single_page_region_keeps_inline_metadata() {
    let heap = TestHeap::new(1);
    let mut b = BuddyAllocator::<PAGE>::new();
    b.init_region(heap.base(), heap.size()).unwrap();
    assert_eq!(b.allocate_pages(1, PAGE), Ok(heap.base()));
    assert_eq!(b.free_pages(), 0);
    b.deallocate_pages(heap.base(), 1);
    assert_eq!(b.free_pages(), 1);
}

#[def_test]
fn test_alloc_at_preserves_each_unselected_page_once() {
    let heap = TestHeap::new(64);
    let mut b = BuddyAllocator::<PAGE>::new();
    b.init_region(heap.base(), heap.size()).unwrap();
    let target = heap.base() + 20 * PAGE;
    assert_eq!(b.allocate_pages_at(target, 4, PAGE), Ok(target));
    assert_eq!(b.free_pages(), 60);
    let mut addrs = alloc::vec::Vec::new();
    while let Ok(addr) = b.allocate_pages(1, PAGE) {
        assert!(!(target..target + 4 * PAGE).contains(&addr));
        addrs.push(addr);
        assert!(addrs.len() <= 60);
    }
    addrs.sort_unstable();
    assert_eq!(addrs.len(), 60);
    assert!(!addrs.windows(2).any(|pair| pair[0] == pair[1]));
    for addr in addrs {
        b.deallocate_pages(addr, 1);
    }
    b.deallocate_pages(target, 4);
    assert_eq!(b.allocate_pages(64, PAGE), Ok(heap.base()));
    b.deallocate_pages(heap.base(), 64);
}

#[def_test]
fn test_alignment_and_region_edges_preserve_free_chunks() {
    let heap = TestHeap::new(64);
    let mut b = BuddyAllocator::<PAGE>::new();
    b.init_region(heap.base() + PAGE, 62 * PAGE).unwrap();
    assert!(b.allocate_pages_at(heap.base() + PAGE, 4, PAGE).is_err());
    assert_eq!(b.free_pages(), 62);
    let aligned = b.allocate_pages(1, 16 * PAGE).unwrap();
    assert!(aligned.is_multiple_of(16 * PAGE));
    b.deallocate_pages(aligned, 1);
    let mut addrs = alloc::vec::Vec::new();
    while let Ok(addr) = b.allocate_pages(1, PAGE) {
        assert!((heap.base() + PAGE..heap.base() + 63 * PAGE).contains(&addr));
        addrs.push(addr);
    }
    assert_eq!(addrs.len(), 62);
    for addr in addrs {
        b.deallocate_pages(addr, 1);
    }
    assert_eq!(b.free_pages(), 62);
}

#[def_test]
fn test_split_to_chunks_empty() {
    // zero size returns no chunks.
    let chunks: alloc::vec::Vec<_> = split_to_chunks::<PAGE>(0x1000, 0).collect();
    assert!(chunks.is_empty());
}

#[def_test]
fn test_split_to_chunks_power_of_two() {
    // A 4-page range at a 4-page-aligned address yields a single order-2 chunk.
    let chunks: alloc::vec::Vec<_> = split_to_chunks::<PAGE>(0x0, 4 * PAGE).collect();
    assert_eq!(chunks, alloc::vec![(0x0, 2)]);
}

#[def_test]
fn test_split_to_chunks_non_power_of_two() {
    // 3 pages starting at 0x3000 (page 3):
    // addr 0x3000, size 0x3000.
    // max_ord(0x3000) = 12-12 = 0  (page 3 is not 2-page aligned)
    // max_by_size(3 pages) = floor(log2(3)) = 1
    // order = min(0, 1) = 0  → (0x3000, 0) 1 page
    // remaining: addr 0x4000, size 0x2000
    // max_ord(0x4000) = 14-12 = 2
    // max_by_size(2 pages) = 1
    // order = min(2, 1) = 1  → (0x4000, 1) 2 pages
    let chunks: alloc::vec::Vec<_> = split_to_chunks::<PAGE>(0x3000, 3 * PAGE).collect();
    assert_eq!(chunks, alloc::vec![(0x3000, 0), (0x4000, 1)]);
}

#[def_test]
fn test_split_to_chunks_six_pages_unaligned() {
    // 6 pages starting at page 7 (addr 0x7000):
    // max_ord(0x7000) = 12-12 = 0, pages=6→max_by_size=2
    // → (0x7000, 0) 1 page
    // addr 0x8000, size 0x5000
    // max_ord(0x8000) = 15-12 = 3, pages=5→max_by_size=2
    // → (0x8000, 2) 4 pages
    // addr 0xC000, size 0x1000
    // → (0xC000, 0) 1 page
    let chunks: alloc::vec::Vec<_> = split_to_chunks::<PAGE>(0x7000, 6 * PAGE).collect();
    assert_eq!(chunks, alloc::vec![(0x7000, 0), (0x8000, 2), (0xC000, 0)]);
}

#[def_test]
fn test_split_to_chunks_recombine_after_alloc() {
    // Allocate 3 pages, free them via split_to_chunks, then re-allocate.
    // The buddy should merge chunks back so that a 3-page request succeeds.
    let (_heap, mut b) = make_alloc();
    let addr = b.allocate_pages(3, PAGE).unwrap();
    // Decompose the 3-page range and return each chunk.
    let chunks: alloc::vec::Vec<_> = split_to_chunks::<PAGE>(addr, 3 * PAGE).collect();
    for (c_addr, order) in &chunks {
        b.deallocate_pages(*c_addr, 1 << order);
    }
    // Re-allocate 3 pages (buddy allocates one order-2 chunk, 4 pages).
    // The 4th page from the first alloc (addr+3*PAGE) is still allocated,
    // so free_pages is 255, not 256.
    let addr2 = b.allocate_pages(3, PAGE).unwrap();
    b.deallocate_pages(addr2, 3);
    assert_eq!(b.free_pages(), 255);
}

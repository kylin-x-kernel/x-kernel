// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Slab-based byte allocator for kernel heap.
//!
//! Implements a size-class slab allocator backed by an internal buddy page
//! allocator. Small allocations (≤ 2048 bytes) are served from fixed-size
//! slabs; larger allocations fall through to the buddy allocator directly.
//!
//! References: Asterinas `heap-allocator/src/allocator.rs`,
//!             `ax_slab_allocator/src/lib.rs`.

use core::alloc::Layout;

use crate::{AllocError, buddy_alloc::BuddyAllocator};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Internal page size used for expanding slabs.
const PAGE_SIZE: usize = 4096;

/// Maximum size served by a slab. Larger allocations go to the buddy.
const MAX_SLAB_SIZE: usize = 2048;

/// Sentinel: freelist is empty.
const LIST_EMPTY: usize = 0;

// ---------------------------------------------------------------------------
// Slab — one size class
// ---------------------------------------------------------------------------

/// One size-class slab.
struct Slab<const BLK_SIZE: usize> {
    /// Total number of blocks ever carved out for this slab (for stats).
    total_blocks: usize,
    /// Head of the intrusive freelist. `LIST_EMPTY` means empty.
    free_head: usize,
}

impl<const BLK_SIZE: usize> Slab<BLK_SIZE> {
    const fn new() -> Self {
        Self {
            total_blocks: 0,
            free_head: LIST_EMPTY,
        }
    }

    fn total_bytes(&self) -> usize {
        self.total_blocks * BLK_SIZE
    }

    fn used_blocks(&self) -> usize {
        let free = self.count_free();
        self.total_blocks.saturating_sub(free)
    }

    fn used_bytes(&self) -> usize {
        self.used_blocks() * BLK_SIZE
    }

    fn count_free(&self) -> usize {
        let mut count = 0usize;
        let mut cur = self.free_head;
        while cur != LIST_EMPTY {
            count += 1;
            // SAFETY: cur is on the slab freelist; each free block
            // stores its next pointer at offset 0.
            cur = unsafe { Slab::<BLK_SIZE>::read_next(cur) };
        }
        count
    }

    /// Pop a block from the freelist. Returns `None` if empty.
    fn pop(&mut self) -> Option<usize> {
        if self.free_head == LIST_EMPTY {
            return None;
        }
        let addr = self.free_head;
        // SAFETY: addr was the head of the freelist; its first bytes
        // store the next free block pointer.
        self.free_head = unsafe { Self::read_next(addr) };
        Some(addr)
    }

    /// Push a block back onto the freelist.
    fn push(&mut self, addr: usize) {
        // SAFETY: addr is the freed block; writing the next pointer
        // into its first bytes is safe — the block is no longer in use.
        unsafe { Self::write_next(addr, self.free_head) };
        self.free_head = addr;
    }

    /// Expand the slab by carving `num_blocks` out of `base`.
    fn expand_from(&mut self, base: usize, num_blocks: usize) {
        for i in 0..num_blocks {
            self.push(base + i * BLK_SIZE);
        }
        self.total_blocks += num_blocks;
    }

    // --- intrusive freelist helpers ---

    unsafe fn read_next(addr: usize) -> usize {
        // SAFETY: caller guarantees addr points to a free slab block whose
        // first bytes store the next pointer.
        unsafe { *(addr as *const usize) }
    }

    unsafe fn write_next(addr: usize, next: usize) {
        // SAFETY: caller guarantees addr points to a free slab block;
        // writing the next pointer there is safe.
        unsafe { (addr as *mut usize).write(next) };
    }
}

// ---------------------------------------------------------------------------
// SlabHeap
// ---------------------------------------------------------------------------

/// Size-class slab byte allocator.
///
/// Manages a fixed memory region via an internal buddy page allocator.
/// Allocations are served from the slab whose block size is the smallest
/// that fits the request.
pub struct SlabHeap {
    slab_8: Slab<8>,
    slab_16: Slab<16>,
    slab_32: Slab<32>,
    slab_64: Slab<64>,
    slab_128: Slab<128>,
    slab_256: Slab<256>,
    slab_512: Slab<512>,
    slab_1024: Slab<1024>,
    slab_2048: Slab<2048>,
    /// Internal buddy for managing the region and large allocations.
    buddy: BuddyAllocator<PAGE_SIZE>,
}

impl SlabHeap {
    // ------------------------------------------------------------------
    // Construction
    // ------------------------------------------------------------------

    /// Creates a new heap backed by memory at `[start, start + size)`.
    ///
    /// # Safety
    ///
    /// The caller must ensure the memory range is valid, not used for
    /// anything else, and remains accessible for the lifetime of the heap.
    pub unsafe fn new(start: usize, size: usize) -> Self {
        let mut heap = Self {
            slab_8: Slab::new(),
            slab_16: Slab::new(),
            slab_32: Slab::new(),
            slab_64: Slab::new(),
            slab_128: Slab::new(),
            slab_256: Slab::new(),
            slab_512: Slab::new(),
            slab_1024: Slab::new(),
            slab_2048: Slab::new(),
            buddy: BuddyAllocator::new(),
        };
        heap.buddy.init_region(start, size).unwrap_or_else(|e| {
            panic!(
                "SlabHeap::new init_region({:#x}, {:#x}) failed: {:?}",
                start, size, e
            )
        });
        heap
    }

    /// Adds more memory to the heap.
    ///
    /// # Safety
    ///
    /// The caller must ensure the memory range is valid and does not
    /// overlap with existing managed memory.
    pub unsafe fn add_memory(&mut self, start: usize, size: usize) {
        self.buddy.add_region(start, size).unwrap_or_else(|e| {
            panic!(
                "SlabHeap::add_memory add_region({:#x}, {:#x}) failed: {:?}",
                start, size, e
            )
        });
    }

    // ------------------------------------------------------------------
    // Allocation
    // ------------------------------------------------------------------

    /// Allocates memory with the given layout.
    ///
    /// Returns the address of the allocated block on success.
    pub fn allocate(&mut self, layout: Layout) -> Result<usize, AllocError> {
        let SlabHeap {
            slab_8,
            slab_16,
            slab_32,
            slab_64,
            slab_128,
            slab_256,
            slab_512,
            slab_1024,
            slab_2048,
            buddy,
        } = self;

        let class = size_class(layout);
        match class {
            SizeClass::Slab8 => slab_alloc::<8>(buddy, slab_8),
            SizeClass::Slab16 => slab_alloc::<16>(buddy, slab_16),
            SizeClass::Slab32 => slab_alloc::<32>(buddy, slab_32),
            SizeClass::Slab64 => slab_alloc::<64>(buddy, slab_64),
            SizeClass::Slab128 => slab_alloc::<128>(buddy, slab_128),
            SizeClass::Slab256 => slab_alloc::<256>(buddy, slab_256),
            SizeClass::Slab512 => slab_alloc::<512>(buddy, slab_512),
            SizeClass::Slab1024 => slab_alloc::<1024>(buddy, slab_1024),
            SizeClass::Slab2048 => slab_alloc::<2048>(buddy, slab_2048),
            SizeClass::Large => {
                let pages = layout.size().div_ceil(PAGE_SIZE);
                let align = layout.align().max(PAGE_SIZE);
                buddy.allocate_pages(pages, align)
            }
        }
    }
}

/// Allocate from a slab, expanding from the buddy if empty.
fn slab_alloc<const BLK_SIZE: usize>(
    buddy: &mut BuddyAllocator<PAGE_SIZE>,
    slab: &mut Slab<BLK_SIZE>,
) -> Result<usize, AllocError> {
    if let Some(addr) = slab.pop() {
        return Ok(addr);
    }

    // Expand: allocate enough pages to cover at least one block.
    let expand_bytes = PAGE_SIZE.max(BLK_SIZE);
    let expand_pages = expand_bytes.div_ceil(PAGE_SIZE);
    let base = buddy
        .allocate_pages(expand_pages, PAGE_SIZE)
        .map_err(|_| AllocError::NoMemory)?;
    let num_blocks = (expand_pages * PAGE_SIZE) / BLK_SIZE;
    slab.expand_from(base, num_blocks);
    slab.pop().ok_or(AllocError::NoMemory)
}

impl SlabHeap {
    // ------------------------------------------------------------------
    // Deallocation
    // ------------------------------------------------------------------

    /// Frees a block previously allocated by [`allocate`](Self::allocate).
    ///
    /// # Safety
    ///
    /// `ptr` must have been returned by a previous call to `allocate`
    /// with the same `layout`.
    pub unsafe fn deallocate(&mut self, ptr: usize, layout: Layout) {
        let SlabHeap {
            slab_8,
            slab_16,
            slab_32,
            slab_64,
            slab_128,
            slab_256,
            slab_512,
            slab_1024,
            slab_2048,
            buddy,
        } = self;

        let class = size_class(layout);
        match class {
            SizeClass::Slab8 => slab_8.push(ptr),
            SizeClass::Slab16 => slab_16.push(ptr),
            SizeClass::Slab32 => slab_32.push(ptr),
            SizeClass::Slab64 => slab_64.push(ptr),
            SizeClass::Slab128 => slab_128.push(ptr),
            SizeClass::Slab256 => slab_256.push(ptr),
            SizeClass::Slab512 => slab_512.push(ptr),
            SizeClass::Slab1024 => slab_1024.push(ptr),
            SizeClass::Slab2048 => slab_2048.push(ptr),
            SizeClass::Large => {
                let pages = layout.size().div_ceil(PAGE_SIZE);
                buddy.deallocate_pages(ptr, pages);
            }
        }
    }

    // ------------------------------------------------------------------
    // Statistics
    // ------------------------------------------------------------------

    /// Total managed heap size in bytes.
    pub fn total_bytes(&self) -> usize {
        self.buddy.managed_bytes()
    }

    /// Currently allocated bytes.
    pub fn used_bytes(&self) -> usize {
        self.slab_8.used_bytes()
            + self.slab_16.used_bytes()
            + self.slab_32.used_bytes()
            + self.slab_64.used_bytes()
            + self.slab_128.used_bytes()
            + self.slab_256.used_bytes()
            + self.slab_512.used_bytes()
            + self.slab_1024.used_bytes()
            + self.slab_2048.used_bytes()
            + self
                .buddy
                .allocated_bytes()
                .saturating_sub(self.slab_total_bytes())
    }

    /// Available bytes.
    pub fn available_bytes(&self) -> usize {
        self.total_bytes().saturating_sub(self.used_bytes())
    }

    fn slab_total_bytes(&self) -> usize {
        self.slab_8.total_bytes()
            + self.slab_16.total_bytes()
            + self.slab_32.total_bytes()
            + self.slab_64.total_bytes()
            + self.slab_128.total_bytes()
            + self.slab_256.total_bytes()
            + self.slab_512.total_bytes()
            + self.slab_1024.total_bytes()
            + self.slab_2048.total_bytes()
    }
}

// ---------------------------------------------------------------------------
// Size-class routing
// ---------------------------------------------------------------------------

enum SizeClass {
    Slab8,
    Slab16,
    Slab32,
    Slab64,
    Slab128,
    Slab256,
    Slab512,
    Slab1024,
    Slab2048,
    Large,
}

/// Map a layout to the smallest slab that fits it.
fn size_class(layout: Layout) -> SizeClass {
    let size = layout.size();
    let align = layout.align();

    if size > MAX_SLAB_SIZE || align > MAX_SLAB_SIZE {
        return SizeClass::Large;
    }

    if size <= 8 && align <= 8 {
        SizeClass::Slab8
    } else if size <= 16 && align <= 16 {
        SizeClass::Slab16
    } else if size <= 32 && align <= 32 {
        SizeClass::Slab32
    } else if size <= 64 && align <= 64 {
        SizeClass::Slab64
    } else if size <= 128 && align <= 128 {
        SizeClass::Slab128
    } else if size <= 256 && align <= 256 {
        SizeClass::Slab256
    } else if size <= 512 && align <= 512 {
        SizeClass::Slab512
    } else if size <= 1024 && align <= 1024 {
        SizeClass::Slab1024
    } else if size <= 2048 && align <= 2048 {
        SizeClass::Slab2048
    } else {
        SizeClass::Large
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(unittest)]
#[allow(missing_docs)]
mod tests {
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
}

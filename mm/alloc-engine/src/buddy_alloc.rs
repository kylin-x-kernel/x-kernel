// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Buddy page allocator.
//!
//! Manages contiguous physical memory regions using the binary buddy algorithm.
//! Each order-`k` free chunk stores an intrusive link to the next free chunk of
//! the same order, so no external metadata arrays are needed.
//!
//! References: Asterinas `frame-allocator/src/set.rs` (BuddySet), Linux `mm/page_alloc.c`.

use crate::{AllocError, AllocResult};

/// Maximum buddy order. With 4 KiB pages this gives 2^20 × 4 KiB = 4 GiB blocks.
const MAX_ORDER: usize = 20;

/// Up to 8 separate memory regions can be registered.
const MAX_REGIONS: usize = 64;

/// Sentinel: no free chunk at this order.
const LIST_EMPTY: usize = 0;

/// Number of pages in a chunk of the given order.
const fn order_pages(order: usize) -> usize {
    1 << order
}

/// Byte size of a chunk of the given order.
const fn order_size<const PAGE_SIZE: usize>(order: usize) -> usize {
    order_pages(order) * PAGE_SIZE
}

// ---------------------------------------------------------------------------
// Intrusive free-node stored in the first bytes of every free chunk
// ---------------------------------------------------------------------------

/// Layout of a free chunk's header (written into the chunk itself).
/// The first 8 bytes store the next pointer; the following `usize` stores the order.
struct FreeNode;

impl FreeNode {
    const ORDER_OFFSET: usize = core::mem::size_of::<usize>();

    /// Read the next pointer from a free chunk.
    ///
    /// # Safety
    /// `addr` must point to a valid, writable free chunk within a managed region.
    unsafe fn read_next(addr: usize) -> usize {
        // SAFETY: caller guarantees addr is a valid free chunk address.
        unsafe { *(addr as *const usize) }
    }

    /// Write the next pointer into a free chunk.
    ///
    /// # Safety
    /// `addr` must point to a valid, writable free chunk within a managed region.
    unsafe fn write_next(addr: usize, next: usize) {
        // SAFETY: caller guarantees addr is a valid free chunk address.
        unsafe { (addr as *mut usize).write(next) };
    }

    /// Write the order into a free chunk.
    ///
    /// # Safety
    /// `addr` must point to a valid, writable free chunk within a managed region.
    unsafe fn write_order(addr: usize, order: usize) {
        // SAFETY: caller guarantees addr is within a valid free chunk.
        unsafe { ((addr + Self::ORDER_OFFSET) as *mut usize).write(order) };
    }

    /// Write both fields atomically from the caller's perspective.
    ///
    /// # Safety
    /// `addr` must point to a valid, writable free chunk within a managed region.
    unsafe fn write(addr: usize, next: usize, order: usize) {
        // SAFETY: caller guarantees addr is a valid free chunk address.
        unsafe {
            Self::write_next(addr, next);
            Self::write_order(addr, order);
        }
    }
}

// ---------------------------------------------------------------------------
// Region
// ---------------------------------------------------------------------------

/// One managed contiguous memory region.
struct Region {
    /// Start address of the allocatable heap (page-aligned).
    heap_start: usize,
    /// Number of pages in the heap.
    total_pages: usize,
    /// Number of currently free pages.
    free_pages: usize,
    /// Per-order freelist heads. `LIST_EMPTY` means the list is empty.
    free_lists: [usize; MAX_ORDER + 1],
}

impl Region {
    fn new(heap_start: usize, total_pages: usize) -> Self {
        Self {
            heap_start,
            total_pages,
            free_pages: 0,
            free_lists: [LIST_EMPTY; MAX_ORDER + 1],
        }
    }

    fn contains<const PAGE_SIZE: usize>(&self, addr: usize) -> bool {
        addr >= self.heap_start && addr < self.heap_start + self.total_pages * PAGE_SIZE
    }

    /// Push a free chunk of the given `order` onto the appropriate freelist.
    fn push_free<const PAGE_SIZE: usize>(&mut self, addr: usize, order: usize) {
        debug_assert!(order <= MAX_ORDER);
        debug_assert!(
            addr >= self.heap_start,
            "push_free: addr {:#x} below heap_start {:#x}",
            addr,
            self.heap_start
        );
        debug_assert!(
            addr + order_size::<PAGE_SIZE>(order) <= self.heap_start + self.total_pages * PAGE_SIZE,
            "push_free: chunk @ {:#x} order {} exceeds region end",
            addr,
            order
        );

        let prev_head = self.free_lists[order];
        // SAFETY: addr is page-aligned, within region bounds, and the chunk is
        // not currently in use — writing the free-node header is safe.
        unsafe { FreeNode::write(addr, prev_head, order) };
        self.free_lists[order] = addr;
        self.free_pages += order_pages(order);
    }

    /// Pop a free chunk of the given `order`. Returns `None` if empty.
    fn pop_free<const PAGE_SIZE: usize>(&mut self, order: usize) -> Option<usize> {
        let head = self.free_lists[order];
        if head == LIST_EMPTY {
            return None;
        }
        // SAFETY: head was just verified non-empty; it points to a free chunk
        // whose first bytes store the next pointer.
        let next = unsafe { FreeNode::read_next(head) };
        self.free_lists[order] = next;
        self.free_pages -= order_pages(order);
        Some(head)
    }

    /// Allocate a chunk of `order` pages from this region.
    fn alloc_order<const PAGE_SIZE: usize>(&mut self, order: usize) -> Option<usize> {
        // Find the smallest non-empty order >= requested order.
        let mut src_order = order;
        while src_order <= MAX_ORDER && self.free_lists[src_order] == LIST_EMPTY {
            src_order += 1;
        }
        if src_order > MAX_ORDER {
            return None;
        }

        // Pop from the found order.
        let chunk = self.pop_free::<PAGE_SIZE>(src_order).unwrap();

        // Split down to the requested order, pushing right buddies.
        let addr = chunk;
        let mut cur_order = src_order;
        while cur_order > order {
            cur_order -= 1;
            let right = addr + order_size::<PAGE_SIZE>(cur_order);
            self.push_free::<PAGE_SIZE>(right, cur_order);
            // addr stays as the left half.
        }

        Some(addr)
    }

    /// Try to find a free chunk at exactly `target` with the given order.
    /// Used by `allocate_pages_at`.
    fn alloc_at<const PAGE_SIZE: usize>(&mut self, target: usize, order: usize) -> Option<usize> {
        if !self.contains::<PAGE_SIZE>(target) {
            return None;
        }

        // Try to find a free block that covers `target`.
        for src_order in order..=MAX_ORDER {
            let mut prev: usize = LIST_EMPTY;
            let mut cur = self.free_lists[src_order];
            while cur != LIST_EMPTY {
                // SAFETY: cur is a free chunk address popped from the freelist;
                // its first bytes hold the next pointer.
                let next = unsafe { FreeNode::read_next(cur) };
                let chunk_end = cur + order_size::<PAGE_SIZE>(src_order);
                if target >= cur && target < chunk_end {
                    // Found a chunk that covers the target. Remove from list.
                    if prev == LIST_EMPTY {
                        self.free_lists[src_order] = next;
                    } else {
                        // SAFETY: prev is another free chunk; writing its next
                        // pointer is safe for the same reason as read_next.
                        unsafe { FreeNode::write_next(prev, next) };
                    }
                    self.free_pages -= order_pages(src_order);

                    // Split into three parts: left, middle (target), right.
                    // Left part (from cur to target, if any).
                    let mut left = cur;
                    let mut lo = src_order;
                    while lo > order && left < target {
                        lo -= 1;
                        let half = left + order_size::<PAGE_SIZE>(lo);
                        if target >= half {
                            // target is in the right half; left half goes back.
                            self.push_free::<PAGE_SIZE>(left, lo);
                            left = half;
                        }
                        // else target is in the left half; right half goes back
                        else {
                            self.push_free::<PAGE_SIZE>(half, lo);
                        }
                    }
                    debug_assert_eq!(left, target);
                    // Right part (from target + size to end, if any).
                    let target_end = target + order_size::<PAGE_SIZE>(order);
                    let right = target_end;
                    let remaining_size = cur + order_size::<PAGE_SIZE>(src_order) - target_end;
                    if remaining_size > 0 {
                        // Split the remainder into maximal-order chunks.
                        let mut r_off = 0usize;
                        let mut r_order = MAX_ORDER;
                        while r_off < remaining_size / PAGE_SIZE {
                            let chunk_pages = 1 << r_order;
                            while r_order > 0
                                && (r_off + chunk_pages > remaining_size / PAGE_SIZE
                                    || !right.is_multiple_of(order_size::<PAGE_SIZE>(r_order)))
                            {
                                r_order -= 1;
                            }
                            self.push_free::<PAGE_SIZE>(right + r_off * PAGE_SIZE, r_order);
                            r_off += 1 << r_order;
                        }
                    }
                    return Some(target);
                }
                prev = cur;
                cur = next;
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------
// BuddyAllocator
// ---------------------------------------------------------------------------

/// A binary-buddy page allocator.
///
/// `PAGE_SIZE` must be a power of two (commonly 0x1000 = 4 KiB).
pub struct BuddyAllocator<const PAGE_SIZE: usize> {
    regions: [Option<Region>; MAX_REGIONS],
    region_count: usize,
}

impl<const PAGE_SIZE: usize> BuddyAllocator<PAGE_SIZE> {
    // ------------------------------------------------------------------
    // Construction
    // ------------------------------------------------------------------

    /// Create an uninitialised allocator.
    pub const fn new() -> Self {
        Self {
            regions: [const { None }; MAX_REGIONS],
            region_count: 0,
        }
    }
}

impl<const PAGE_SIZE: usize> Default for BuddyAllocator<PAGE_SIZE> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const PAGE_SIZE: usize> BuddyAllocator<PAGE_SIZE> {
    // ------------------------------------------------------------------
    // Region management
    // ------------------------------------------------------------------

    /// Initialize the allocator with a memory region.
    pub fn init_region(&mut self, base: usize, size: usize) -> AllocResult {
        self.reset();
        self.add_region(base, size)
    }

    /// Add a memory region to the allocator.
    pub fn add_region(&mut self, base: usize, size: usize) -> AllocResult {
        if self.region_count >= MAX_REGIONS {
            return Err(AllocError::NoMemory);
        }
        if size < PAGE_SIZE || !base.is_multiple_of(PAGE_SIZE) {
            return Err(AllocError::InvalidInput);
        }

        let heap_start = base;
        let heap_size = size - (size % PAGE_SIZE);
        if heap_size == 0 {
            return Err(AllocError::InvalidInput);
        }
        let total_pages = heap_size / PAGE_SIZE;

        // Check overlap with existing regions.
        let heap_end = heap_start + heap_size;
        for i in 0..self.region_count {
            if let Some(ref r) = self.regions[i] {
                let r_end = r.heap_start + r.total_pages * PAGE_SIZE;
                if heap_start < r_end && r.heap_start < heap_end {
                    return Err(AllocError::MemoryOverlap);
                }
            }
        }

        let mut region = Region::new(heap_start, total_pages);

        // Break the region into maximal-order buddy chunks.
        let mut offset = 0usize;
        while offset < total_pages {
            let mut order = MAX_ORDER;
            loop {
                let chunk_pages = 1 << order;
                let addr = heap_start + offset * PAGE_SIZE;
                if chunk_pages <= total_pages - offset
                    && addr.is_multiple_of(order_size::<PAGE_SIZE>(order))
                {
                    break;
                }
                if order == 0 {
                    break;
                }
                order -= 1;
            }
            let addr = heap_start + offset * PAGE_SIZE;
            region.push_free::<PAGE_SIZE>(addr, order);
            offset += 1 << order;
        }

        self.regions[self.region_count] = Some(region);
        self.region_count += 1;
        Ok(())
    }

    /// Reset the allocator (discard all regions).
    pub fn reset(&mut self) {
        for i in 0..self.region_count {
            self.regions[i] = None;
        }
        self.region_count = 0;
    }

    // ------------------------------------------------------------------
    // Page allocation
    // ------------------------------------------------------------------

    /// Allocate `count` contiguous pages.
    ///
    /// `align_pow2` must be at least `PAGE_SIZE` and a power of two.
    pub fn allocate_pages(&mut self, count: usize, align_pow2: usize) -> AllocResult<usize> {
        if count == 0 {
            return Err(AllocError::InvalidInput);
        }
        let align = if align_pow2 == 0 {
            PAGE_SIZE
        } else {
            align_pow2
        };
        if !align.is_power_of_two() || align < PAGE_SIZE {
            return Err(AllocError::InvalidInput);
        }

        let order = count.next_power_of_two().trailing_zeros() as usize;
        if order > MAX_ORDER {
            return Err(AllocError::InvalidInput);
        }

        // align_pages determines how alignment constrains the search.
        let align_pages = align / PAGE_SIZE;

        for i in 0..self.region_count {
            let region = self.regions[i].as_mut().unwrap();
            if let Some(addr) = Self::alloc_aligned::<PAGE_SIZE>(region, order, align_pages) {
                return Ok(addr);
            }
        }

        Err(AllocError::NoMemory)
    }

    fn alloc_aligned<const PS: usize>(
        region: &mut Region,
        order: usize,
        align_pages: usize,
    ) -> Option<usize> {
        if align_pages <= order_pages(order) {
            // Simple case: the chunk itself is large enough for alignment.
            // The buddy allocator naturally returns properly aligned chunks.
            return region.alloc_order::<PS>(order);
        }

        // Larger alignment than chunk size: need to find a bigger block
        // and split it so the target sub-block is aligned.
        for src_order in order..=MAX_ORDER {
            let mut prev: usize = LIST_EMPTY;
            let mut cur = region.free_lists[src_order];
            while cur != LIST_EMPTY {
                // SAFETY: cur is a free chunk address from the freelist;
                // its first bytes store the next pointer.
                let next = unsafe { FreeNode::read_next(cur) };
                if cur.is_multiple_of(align_pages * PS) {
                    // Remove from list.
                    if prev == LIST_EMPTY {
                        region.free_lists[src_order] = next;
                    } else {
                        // SAFETY: prev is another free chunk; updating its
                        // next pointer is safe.
                        unsafe { FreeNode::write_next(prev, next) };
                    }
                    region.free_pages -= order_pages(src_order);

                    // Split down.
                    let addr = cur;
                    let mut cur_order = src_order;
                    while cur_order > order {
                        cur_order -= 1;
                        region.push_free::<PS>(addr + order_size::<PS>(cur_order), cur_order);
                    }
                    return Some(addr);
                }
                prev = cur;
                cur = next;
            }
        }
        None
    }

    /// Free `count` pages at `addr`.
    pub fn deallocate_pages(&mut self, addr: usize, count: usize) {
        if count == 0 {
            return;
        }
        let order = count.next_power_of_two().trailing_zeros() as usize;
        let region = self.find_region_mut::<PAGE_SIZE>(addr);
        let Some(region) = region else {
            return;
        };

        let mut cur_order = order;
        let mut cur_addr = addr;

        // Try to merge with buddy repeatedly.
        while cur_order < MAX_ORDER {
            let buddy = cur_addr ^ order_size::<PAGE_SIZE>(cur_order);
            // Check if buddy is free and of the same order.
            let found = {
                let mut prev: usize = LIST_EMPTY;
                let mut cur = region.free_lists[cur_order];
                let mut found = false;
                while cur != LIST_EMPTY {
                    if cur == buddy {
                        // Remove buddy from freelist.
                        // SAFETY: cur is a free chunk; reading its next
                        // pointer is safe.
                        let next = unsafe { FreeNode::read_next(cur) };
                        if prev == LIST_EMPTY {
                            region.free_lists[cur_order] = next;
                        } else {
                            // SAFETY: prev is another free chunk; updating
                            // its next pointer is safe.
                            unsafe { FreeNode::write_next(prev, next) };
                        }
                        region.free_pages -= order_pages(cur_order);
                        found = true;
                        break;
                    }
                    prev = cur;
                    // SAFETY: cur is on the freelist; its first bytes are the
                    // next pointer.
                    cur = unsafe { FreeNode::read_next(cur) };
                }
                found
            };
            if !found {
                break;
            }
            cur_addr = cur_addr.min(buddy);
            cur_order += 1;
        }

        region.push_free::<PAGE_SIZE>(cur_addr, cur_order);
    }

    /// Allocate pages at a specific address.
    pub fn allocate_pages_at(
        &mut self,
        base: usize,
        count: usize,
        align_pow2: usize,
    ) -> AllocResult<usize> {
        if count == 0 {
            return Err(AllocError::InvalidInput);
        }
        let align = if align_pow2 == 0 {
            PAGE_SIZE
        } else {
            align_pow2
        };
        if !base.is_multiple_of(align) {
            return Err(AllocError::InvalidInput);
        }

        let order = count.next_power_of_two().trailing_zeros() as usize;
        if order > MAX_ORDER {
            return Err(AllocError::InvalidInput);
        }

        for i in 0..self.region_count {
            let region = self.regions[i].as_mut().unwrap();
            if let Some(addr) = region.alloc_at::<PAGE_SIZE>(base, order) {
                return Ok(addr);
            }
        }

        Err(AllocError::NoMemory)
    }

    // ------------------------------------------------------------------
    // Statistics
    // ------------------------------------------------------------------

    /// Total number of managed pages.
    pub fn total_pages(&self) -> usize {
        let mut total = 0;
        for i in 0..self.region_count {
            total += self.regions[i].as_ref().unwrap().total_pages;
        }
        total
    }

    /// Number of free pages.
    pub fn free_pages(&self) -> usize {
        let mut free = 0;
        for i in 0..self.region_count {
            free += self.regions[i].as_ref().unwrap().free_pages;
        }
        free
    }

    /// Number of allocated pages.
    pub fn used_pages(&self) -> usize {
        self.total_pages().saturating_sub(self.free_pages())
    }

    /// Number of available (free) pages.
    pub fn available_pages(&self) -> usize {
        self.free_pages()
    }

    /// Managed bytes (total heap size across all regions).
    pub fn managed_bytes(&self) -> usize {
        self.total_pages() * PAGE_SIZE
    }

    /// Allocated bytes.
    pub fn allocated_bytes(&self) -> usize {
        self.used_pages() * PAGE_SIZE
    }

    // ------------------------------------------------------------------
    // Slab support
    // ------------------------------------------------------------------

    /// Set page flags on the page containing `addr`. Used by the slab
    /// allocator to mark pages.
    ///
    /// Currently a no-op stub — the xk-alloc buddy does not track per-page
    /// flags. This hook exists for compatibility with the `buddy-slab-allocator`
    /// trait interface.
    pub fn set_page_flags(&mut self, _addr: usize, _flags: PageFlags) -> AllocResult {
        // No per-page metadata; the caller is expected to track slab pages
        // externally if needed.
        Ok(())
    }

    /// Read the flags of the page containing `addr`.
    pub fn page_flags(&self, _addr: usize) -> AllocResult<PageFlags> {
        Ok(PageFlags::Allocated)
    }

    // ------------------------------------------------------------------
    // Internals
    // ------------------------------------------------------------------

    fn find_region_mut<const PS: usize>(&mut self, addr: usize) -> Option<&mut Region> {
        for i in 0..self.region_count {
            if self.regions[i].as_ref().unwrap().contains::<PS>(addr) {
                return self.regions[i].as_mut();
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------
// PageFlags (minimal — compatibility with buddy-slab-allocator)
// ---------------------------------------------------------------------------

/// Page allocation state (minimal subset of buddy-slab-allocator's `PageFlags`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PageFlags {
    /// Page is free.
    Free      = 0,
    /// Page is allocated.
    Allocated = 1,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(unittest)]
#[allow(missing_docs)]
mod tests {
    use unittest::def_test;

    use super::*;

    const PAGE: usize = 4096;

    fn make_alloc() -> BuddyAllocator<PAGE> {
        let mut b = BuddyAllocator::<PAGE>::new();
        // 256 pages = 1 MiB heap, plenty for tests.
        let heap = alloc::vec![0u8; 256 * PAGE];
        let base = heap.as_ptr() as usize;
        let size = heap.len();
        b.init_region(base, size).unwrap();
        core::mem::forget(heap); // Don't drop — the allocator owns it.
        b
    }

    #[def_test]
    fn test_init_and_stats() {
        let b = make_alloc();
        assert_eq!(b.total_pages(), 256);
        assert_eq!(b.free_pages(), 256);
        assert_eq!(b.used_pages(), 0);
    }

    #[def_test]
    fn test_alloc_one_page() {
        let mut b = make_alloc();
        let addr = b.allocate_pages(1, PAGE).unwrap();
        assert!(addr.is_multiple_of(PAGE));
        assert_eq!(b.used_pages(), 1);
        assert_eq!(b.free_pages(), 255);
    }

    #[def_test]
    fn test_alloc_dealloc_one_page() {
        let mut b = make_alloc();
        let addr = b.allocate_pages(1, PAGE).unwrap();
        b.deallocate_pages(addr, 1);
        assert_eq!(b.free_pages(), 256);
        assert_eq!(b.used_pages(), 0);
    }

    #[def_test]
    fn test_alloc_multi_page() {
        let mut b = make_alloc();
        let addr = b.allocate_pages(4, PAGE).unwrap(); // order 2
        assert!(addr.is_multiple_of(PAGE));
        assert_eq!(b.used_pages(), 4);
        b.deallocate_pages(addr, 4);
        assert_eq!(b.free_pages(), 256);
    }

    #[def_test]
    fn test_alloc_exhaust_then_free() {
        let mut b = make_alloc();
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
        let mut b = make_alloc();
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
        let mut b = make_alloc();
        let addr = b.allocate_pages(64, PAGE).unwrap(); // order 6
        assert_eq!(b.used_pages(), 64);
        b.deallocate_pages(addr, 64);
        assert_eq!(b.free_pages(), 256);
    }

    #[def_test]
    fn test_add_region() {
        let mut b = BuddyAllocator::<PAGE>::new();
        let heap1 = alloc::vec![0u8; 32 * PAGE];
        let heap2 = alloc::vec![0u8; 64 * PAGE];
        b.init_region(heap1.as_ptr() as usize, heap1.len()).unwrap();
        core::mem::forget(heap1);
        b.add_region(heap2.as_ptr() as usize, heap2.len()).unwrap();
        core::mem::forget(heap2);
        assert_eq!(b.total_pages(), 96);
        assert_eq!(b.free_pages(), 96);
    }

    #[def_test]
    fn test_overlap_rejected() {
        let mut b = BuddyAllocator::<PAGE>::new();
        let heap = alloc::vec![0u8; 64 * PAGE];
        let base = heap.as_ptr() as usize;
        b.init_region(base, 64 * PAGE).unwrap();
        assert!(b.add_region(base, 32 * PAGE).is_err());
        core::mem::forget(heap);
    }
}

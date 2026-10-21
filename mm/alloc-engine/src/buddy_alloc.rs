// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Buddy page allocator.
//!
//! Manages contiguous physical memory regions using the binary buddy algorithm.
//! Free chunks carry intrusive links; an allocator-owned bitmap certifies free
//! chunk heads before their headers are read. Coalescing probes one buddy per
//! order and unlinks it directly, independent of free-list length.

use intrusive_collections::{LinkedList, LinkedListAtomicLink, UnsafeRef, intrusive_adapter};

use crate::{AllocError, AllocResult};

/// Maximum buddy order. With 4 KiB pages this gives 2^20 × 4 KiB = 4 GiB blocks.
const MAX_ORDER: usize = 20;

/// Up to 64 separate memory regions can be registered.
const MAX_REGIONS: usize = 64;

/// Small regions keep their free-head bitmap inside the allocator itself.
const INLINE_BITMAP_WORDS: usize = 4;
const WORD_BITS: usize = usize::BITS as usize;

const fn order_pages(order: usize) -> usize {
    1 << order
}

const fn order_size<const PAGE_SIZE: usize>(order: usize) -> usize {
    order_pages(order) * PAGE_SIZE
}

/// Initialized only while the chunk belongs to a free list.
struct FreeNode {
    link: LinkedListAtomicLink,
    order: usize,
}

// `UnsafeRef` owns nothing: it only lets a list link a chunk whose storage is
// owned by the caller of this allocator and never freed through the list.
intrusive_adapter!(FreeNodeAdapter = UnsafeRef<FreeNode>: FreeNode { link => LinkedListAtomicLink });

/// A set bit certifies an initialized free node, not merely an unused page.
/// Large regions reserve bitmap pages which are never handed to callers.
struct FreeHeadBitmap {
    inline: [usize; INLINE_BITMAP_WORDS],
    /// Zero selects inline storage; otherwise points to the reserved prefix.
    storage: usize,
}

impl FreeHeadBitmap {
    fn word(&self, index: usize) -> usize {
        if self.storage == 0 {
            self.inline[index]
        } else {
            // SAFETY: Region indexes this bitmap only for its usable pages.
            // add_region reserved and zeroed enough aligned words for every
            // registered page, and callers cannot allocate the bitmap prefix.
            let word = unsafe { (self.storage as *const usize).add(index) };
            // SAFETY: add_region initialized this aligned bitmap word; Region's
            // shared borrow prevents concurrent bitmap mutation.
            unsafe { word.read() }
        }
    }

    fn set(&mut self, page: usize, is_free_head: bool) {
        let index = page / WORD_BITS;
        let mask = 1 << (page % WORD_BITS);
        let word = if is_free_head {
            self.word(index) | mask
        } else {
            self.word(index) & !mask
        };
        if self.storage == 0 {
            self.inline[index] = word;
        } else {
            // SAFETY: The index is within the reserved, initialized bitmap;
            // exclusive Region access serializes bitmap and list transitions.
            let destination = unsafe { (self.storage as *mut usize).add(index) };
            // SAFETY: This word is within the reserved bitmap and &mut self
            // excludes other bitmap accesses for the write.
            unsafe { destination.write(word) };
        }
    }

    fn contains(&self, page: usize) -> bool {
        self.word(page / WORD_BITS) & (1 << (page % WORD_BITS)) != 0
    }
}

/// One exclusively owned contiguous region, including its reserved metadata.
struct Region {
    region_start: usize,
    heap_start: usize,
    total_pages: usize,
    free_pages: usize,
    free_lists: [LinkedList<FreeNodeAdapter>; MAX_ORDER + 1],
    free_heads: FreeHeadBitmap,
}

impl Drop for Region {
    fn drop(&mut self) {
        // Free chunks belong to the caller's memory, not to this Region, so
        // retiring a region must not write into them. `LinkedList::drop` clears
        // by walking each list and storing an unlink marker into every node;
        // once the caller reuses or re-registers that memory, those stores land
        // on blocks another allocator has published, leaving a bitmap-certified
        // free head whose link reads as unlinked. A later removal then follows
        // the marker as if it were a node pointer. `fast_clear` drops only the
        // list headers, which is all a retiring Region may touch.
        for list in &mut self.free_lists {
            list.fast_clear();
        }
    }
}

impl Region {
    fn contains<const PAGE_SIZE: usize>(&self, addr: usize) -> bool {
        addr >= self.heap_start && addr < self.heap_start + self.total_pages * PAGE_SIZE
    }

    fn is_free<const PAGE_SIZE: usize>(&self, addr: usize, order: usize) -> bool {
        if !self.contains::<PAGE_SIZE>(addr)
            || !addr.is_multiple_of(PAGE_SIZE)
            || !self
                .free_heads
                .contains((addr - self.heap_start) / PAGE_SIZE)
        {
            return false;
        }
        // SAFETY: The out-of-band bitmap certifies an initialized FreeNode
        // still owned by this region. No allocated page contents are inspected.
        unsafe { (*(addr as *const FreeNode)).order == order }
    }

    fn push_free<const PAGE_SIZE: usize>(&mut self, addr: usize, order: usize) {
        debug_assert!(order <= MAX_ORDER);
        debug_assert!(addr.is_multiple_of(order_size::<PAGE_SIZE>(order)));
        debug_assert!(self.contains::<PAGE_SIZE>(addr));
        debug_assert!(
            addr + order_size::<PAGE_SIZE>(order) <= self.heap_start + self.total_pages * PAGE_SIZE
        );
        let page = (addr - self.heap_start) / PAGE_SIZE;
        debug_assert!(!self.free_heads.contains(page));
        // SAFETY: The allocator exclusively owns this aligned, free chunk.
        // add_region validates that one page holds a FreeNode. The freshly
        // initialized node stays at this address until removed from this list,
        // so the `UnsafeRef` never dangles and owns no storage.
        unsafe {
            (addr as *mut FreeNode).write(FreeNode {
                link: LinkedListAtomicLink::new(),
                order,
            });
        }
        // SAFETY: The node was initialized above in the exclusively owned free
        // chunk, and remains at this address until it is removed from the list.
        let node = unsafe { UnsafeRef::from_raw(addr as *const FreeNode) };
        self.free_lists[order].push_front(node);
        self.free_heads.set(page, true);
        self.free_pages += order_pages(order);
    }

    /// Unlink a known free chunk without searching its order's list.
    fn remove_free<const PAGE_SIZE: usize>(&mut self, addr: usize, order: usize) {
        debug_assert!(self.is_free::<PAGE_SIZE>(addr, order));
        // SAFETY: Callers obtained this address from this order's list or from
        // is_free, which validates bitmap membership and node order. Exclusive
        // Region access keeps the initialized node on that list until removal.
        let removed = unsafe {
            self.free_lists[order]
                .cursor_mut_from_ptr(addr as *const FreeNode)
                .remove()
        };
        debug_assert!(removed.is_some());
        self.free_heads
            .set((addr - self.heap_start) / PAGE_SIZE, false);
        self.free_pages -= order_pages(order);
    }

    fn pop_free<const PAGE_SIZE: usize>(&mut self, order: usize) -> Option<usize> {
        let node = self.free_lists[order].pop_front()?;
        let addr = UnsafeRef::into_raw(node) as usize;
        self.free_heads
            .set((addr - self.heap_start) / PAGE_SIZE, false);
        self.free_pages -= order_pages(order);
        Some(addr)
    }

    fn alloc_order<const PAGE_SIZE: usize>(&mut self, order: usize) -> Option<usize> {
        let src_order = (order..=MAX_ORDER).find(|&i| !self.free_lists[i].is_empty())?;
        let addr = self.pop_free::<PAGE_SIZE>(src_order)?;
        self.split_towards::<PAGE_SIZE>(addr, src_order, addr, order);
        Some(addr)
    }

    /// Return each unselected half once; the target half remains allocated.
    fn split_towards<const PAGE_SIZE: usize>(
        &mut self,
        mut addr: usize,
        mut src_order: usize,
        target: usize,
        order: usize,
    ) {
        while src_order > order {
            src_order -= 1;
            let right = addr + order_size::<PAGE_SIZE>(src_order);
            if target >= right {
                self.push_free::<PAGE_SIZE>(addr, src_order);
                addr = right;
            } else {
                self.push_free::<PAGE_SIZE>(right, src_order);
            }
        }
        debug_assert_eq!(addr, target);
    }

    fn alloc_at<const PAGE_SIZE: usize>(&mut self, target: usize, order: usize) -> Option<usize> {
        if !self.contains::<PAGE_SIZE>(target)
            || !target.is_multiple_of(order_size::<PAGE_SIZE>(order))
        {
            return None;
        }
        for src_order in order..=MAX_ORDER {
            let addr = target & !(order_size::<PAGE_SIZE>(src_order) - 1);
            if self.is_free::<PAGE_SIZE>(addr, src_order) {
                self.remove_free::<PAGE_SIZE>(addr, src_order);
                self.split_towards::<PAGE_SIZE>(addr, src_order, target, order);
                return Some(target);
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

    /// Add exclusively owned, page-aligned writable storage to the allocator.
    ///
    /// Regions larger than the inline bitmap reserve a page-rounded prefix for
    /// one free-head bit per registered page. Statistics and allocations exclude
    /// that prefix. Storage must remain valid until reset and must not overlap
    /// live allocations or another allocator's region.
    pub fn add_region(&mut self, base: usize, size: usize) -> AllocResult {
        if self.region_count >= MAX_REGIONS {
            return Err(AllocError::NoMemory);
        }
        if !PAGE_SIZE.is_power_of_two()
            || PAGE_SIZE < core::mem::size_of::<FreeNode>()
            || base == 0
            || size < PAGE_SIZE
            || !base.is_multiple_of(PAGE_SIZE)
        {
            return Err(AllocError::InvalidInput);
        }
        let heap_size = size - (size % PAGE_SIZE);
        let region_end = base
            .checked_add(heap_size)
            .ok_or(AllocError::InvalidInput)?;
        let registered_pages = heap_size / PAGE_SIZE;
        // Overlap checks include metadata pages, not just allocatable chunks.
        for region in self.regions[..self.region_count].iter().flatten() {
            let end = region.heap_start + region.total_pages * PAGE_SIZE;
            if base < end && region.region_start < region_end {
                return Err(AllocError::MemoryOverlap);
            }
        }
        let metadata_pages = if registered_pages <= INLINE_BITMAP_WORDS * WORD_BITS {
            0
        } else {
            registered_pages.div_ceil(8).div_ceil(PAGE_SIZE)
        };
        let heap_start = base + metadata_pages * PAGE_SIZE;
        let total_pages = registered_pages - metadata_pages;
        if metadata_pages != 0 {
            // SAFETY: The caller supplies exclusive, writable region storage.
            // The checked prefix fits the region and is excluded from the heap.
            unsafe { core::ptr::write_bytes(base as *mut u8, 0, metadata_pages * PAGE_SIZE) };
        }
        let mut region = Region {
            region_start: base,
            heap_start,
            total_pages,
            free_pages: 0,
            free_lists: [const { LinkedList::new(FreeNodeAdapter::NEW) }; MAX_ORDER + 1],
            free_heads: FreeHeadBitmap {
                inline: [0; INLINE_BITMAP_WORDS],
                storage: if metadata_pages == 0 { 0 } else { base },
            },
        };

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

        // Large-alignment allocation retains the existing search policy.
        // Once selected, a node is removed through the same bitmap/list update.
        for src_order in order..=MAX_ORDER {
            let addr = region.free_lists[src_order]
                .iter()
                .map(|node| node as *const FreeNode as usize)
                .find(|addr| addr.is_multiple_of(align_pages * PS));
            if let Some(addr) = addr {
                region.remove_free::<PS>(addr, src_order);
                region.split_towards::<PS>(addr, src_order, addr, order);
                return Some(addr);
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
            if !region.is_free::<PAGE_SIZE>(buddy, cur_order) {
                break;
            }
            region.remove_free::<PAGE_SIZE>(buddy, cur_order);
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

    /// Total number of allocatable pages, excluding reserved bitmap storage.
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

    /// Allocatable bytes across all regions, excluding reserved bitmap storage.
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
// Utility: split a byte range into maximal-order buddy chunks
// ---------------------------------------------------------------------------

/// Split a page-aligned byte range into maximal buddy chunks.
///
/// Each yielded `(addr, order)` pair satisfies:
/// - `addr` is aligned to `(1 << order) * PAGE_SIZE`
/// - the chunk spans exactly `(1 << order)` pages
///
/// This is useful for decomposing arbitrary page-aligned, page-multiple
/// ranges (including non-power-of-two sizes) into chunks the buddy
/// allocator can accept via `deallocate_pages`.
///
/// Returns `None` immediately when `size == 0`.
pub fn split_to_chunks<const PAGE_SIZE: usize>(
    addr: usize,
    size: usize,
) -> impl Iterator<Item = (usize, usize)> {
    assert!(
        addr.is_multiple_of(PAGE_SIZE),
        "split_to_chunks: addr {:#x} not PAGE_SIZE-aligned",
        addr,
    );
    assert!(
        size.is_multiple_of(PAGE_SIZE),
        "split_to_chunks: size {:#x} not a PAGE_SIZE multiple",
        size,
    );

    let mut addr = addr;
    let mut size = size;

    core::iter::from_fn(move || {
        if size == 0 {
            return None;
        }

        // Largest order whose chunk size the address is naturally aligned to.
        let max_ord =
            (addr.trailing_zeros() as usize).saturating_sub(PAGE_SIZE.trailing_zeros() as usize);

        // Largest order whose chunk size does not exceed the remaining size.
        let pages = size / PAGE_SIZE;
        let max_by_size = (usize::BITS as usize).saturating_sub(pages.leading_zeros() as usize + 1);

        let order = max_ord.min(max_by_size);
        let chunk_bytes = (1usize << order) * PAGE_SIZE;
        let chunk_addr = addr;

        addr += chunk_bytes;
        size -= chunk_bytes;

        Some((chunk_addr, order))
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

unittest::path_mod_test!(tests, "../ut-tests/buddy_alloc/tests.rs");

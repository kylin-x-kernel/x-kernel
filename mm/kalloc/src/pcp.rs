// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-CPU page set (PCP) for fast 1–4 page allocation.
//!
//! This module implements a per-CPU page set (`PerCpuPageSet`), analogous
//! to Linux's `struct per_cpu_pageset`.  It contains per-order caches
//! (`PerCpuPages`) for 1, 2, 3, and 4 contiguous pages, designed to
//! reduce contention on the global buddy allocator lock.
//!
//! Pages are bulk-filled from the global pool when a cache is empty and
//! bulk-drained when it is full.
//!
//! Refills first try one contiguous buddy request for the whole batch (low
//! lock hold time, adjacent blocks that drain can merge), then fall back to
//! per-block requests mirroring Linux `rmqueue_bulk()`: the batch size is
//! only a target, each block is an independent allocation, and the batch
//! stops at the first failure, so partial refills are normal under memory
//! pressure. When a refill obtains no block at all, the allocation fails.
//!
//! References:
//! - Asterinas `frame-allocator/src/cache.rs` (CacheOfSizes)
//! - Linux `mm/page_alloc.c` (struct per_cpu_pageset)
//!
//! # Safety
//!
//! All public functions must be called with local IRQs disabled to prevent
//! reentrancy from interrupt handlers on the same CPU.

use alloc_engine::{AllocResult, BuddyPageAllocator, PageAllocator, split_to_chunks};
use memaddr::PAGE_SIZE_4K;

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// Public constants
// ---------------------------------------------------------------------------

/// Maximum number of contiguous pages served by the PCP fast path.
/// Allocations of 1..=PCP_MAX_PAGES are eligible for per-CPU caching.
pub(crate) const PCP_MAX_PAGES: usize = 4;

// ---------------------------------------------------------------------------
// Cache sizes (number of entries per cache), from Asterinas
// ---------------------------------------------------------------------------

/// Max cached 1-page blocks.
const C1: usize = 12;
/// Max cached 2-page blocks.
const C2: usize = 6;
/// Max cached 3-page blocks.
const C3: usize = 6;
/// Max cached 4-page blocks.
const C4: usize = 6;

/// Maximum entries collected during a drain (largest COUNT + 1).
const DRAIN_BUF: usize = C1 + 1; // C1 = 12 is the largest COUNT

// ---------------------------------------------------------------------------
// PerCpuPages — per-CPU cache for a single block size
// ---------------------------------------------------------------------------

/// Per-CPU cache for a single page order.
///
/// Each cached entry holds the start address (physical) of a contiguous
/// block of `NR_PAGES` pages.  At most `COUNT` entries are stored.
///
/// This is analogous to Linux's `struct per_cpu_pages`.
#[derive(Clone, Copy)]
struct PerCpuPages<const NR_PAGES: usize, const COUNT: usize> {
    addrs: [usize; COUNT],
    size: u8,
}

impl<const NR_PAGES: usize, const COUNT: usize> PerCpuPages<NR_PAGES, COUNT> {
    const fn new() -> Self {
        Self {
            addrs: [0; COUNT],
            size: 0,
        }
    }

    /// Byte size of one cached block.
    const fn block_bytes() -> usize {
        NR_PAGES * PAGE_SIZE_4K
    }

    /// Pop a cached address. Returns `None` if empty.
    fn try_pop(&mut self) -> Option<usize> {
        if self.size == 0 {
            return None;
        }
        self.size -= 1;
        Some(self.addrs[self.size as usize])
    }

    /// Push a freed address. Returns `Err(addr)` if full.
    fn try_push(&mut self, addr: usize) -> Result<(), usize> {
        if self.size as usize >= COUNT {
            return Err(addr);
        }
        self.addrs[self.size as usize] = addr;
        self.size += 1;
        Ok(())
    }

    /// Current number of cached entries.
    #[allow(dead_code)]
    fn len(&self) -> usize {
        self.size as usize
    }

    /// Allocate one block, possibly refilling from the buddy allocator.
    ///
    /// Double-checks the cache before refilling: an interrupt may have
    /// consumed the last entry between the outer `try_alloc` and this
    /// `fill_cache` call.
    fn alloc(&mut self, palloc: &mut BuddyPageAllocator<PAGE_SIZE_4K>) -> AllocResult<usize> {
        if let Some(addr) = self.try_pop() {
            return Ok(addr);
        }
        self.refill(palloc)
    }

    /// Free one block, possibly draining to the buddy allocator when full.
    fn dealloc(&mut self, palloc: &mut BuddyPageAllocator<PAGE_SIZE_4K>, addr: usize) {
        if self.try_push(addr).is_err() {
            self.drain(palloc, addr);
        }
    }

    // ------------------------------------------------------------------
    // Internals
    // ------------------------------------------------------------------

    /// Bulk-fill the cache from the global buddy allocator.
    ///
    /// Tries one contiguous buddy request for the whole batch first: a single
    /// allocation keeps the global buddy lock hold time low and leaves the
    /// cached blocks physically adjacent, so a later drain merges them into
    /// large buddy chunks. When that fails (fragmentation), falls back to
    /// per-block requests mirroring Linux `rmqueue_bulk()`: `COUNT * 2/3` is
    /// only a target, the rounding slack of each request is returned to the
    /// buddy, and the batch stops at the first failure, so a fragmented pool
    /// still yields a partial refill. Returns the first block to the caller,
    /// or `Err` only when no block at all could be obtained.
    fn refill(&mut self, palloc: &mut BuddyPageAllocator<PAGE_SIZE_4K>) -> AllocResult<usize> {
        let nr_to_alloc = COUNT * 2 / 3;
        let total_pages = nr_to_alloc * NR_PAGES;

        // Fast path: one contiguous request for the whole batch.
        if let Ok(base) = palloc.allocate_pages(total_pages, PAGE_SIZE_4K) {
            Self::return_slack(palloc, base, total_pages);
            for i in 1..nr_to_alloc {
                let addr = base + i * Self::block_bytes();
                // The cache was empty on entry, so these pushes always succeed.
                let _ = self.try_push(addr);
            }
            return Ok(base);
        }

        // The first block doubles as the caller's allocation. If even this
        // one fails there is nothing to cache.
        let base = palloc
            .allocate_pages(NR_PAGES, PAGE_SIZE_4K)
            .inspect_err(|_| {
                log::warn!(
                    "pcp refill(NR={}, COUNT={}): buddy.allocate_pages({} pages) failed",
                    NR_PAGES,
                    COUNT,
                    NR_PAGES,
                );
            })?;

        Self::return_slack(palloc, base, NR_PAGES);

        // Fill the cache with the remaining blocks, one independent buddy
        // request each, so a later failure never rolls back earlier
        // successes. (The cache was empty on entry, so these pushes always
        // succeed.)
        for _ in 1..nr_to_alloc {
            let Ok(addr) = palloc.allocate_pages(NR_PAGES, PAGE_SIZE_4K) else {
                break;
            };
            Self::return_slack(palloc, addr, NR_PAGES);
            let _ = self.try_push(addr);
        }

        Ok(base)
    }

    /// Return the tail pages that the buddy rounded a `pages`-page request up
    /// to a power-of-two chunk, so a cached block occupies exactly `pages`
    /// pages. This keeps refill and drain bookkeeping in sync: drain
    /// decomposes each block with [`split_to_chunks`] and must not find slack
    /// pages that were never returned.
    fn return_slack(
        palloc: &mut BuddyPageAllocator<PAGE_SIZE_4K>,
        block_addr: usize,
        pages: usize,
    ) {
        let slack_pages = pages.next_power_of_two() - pages;
        if slack_pages == 0 {
            return;
        }
        let slack_addr = block_addr + pages * PAGE_SIZE_4K;
        for (chunk_addr, order) in
            split_to_chunks::<PAGE_SIZE_4K>(slack_addr, slack_pages * PAGE_SIZE_4K)
        {
            palloc.deallocate_pages(chunk_addr, 1 << order);
        }
    }

    /// Bulk-drain entries from the cache back to the buddy allocator.
    ///
    /// Collects the freed block plus up to `COUNT * 2/3` additional
    /// blocks popped from the cache, sorts them by address, merges
    /// physically-adjacent ranges into larger contiguous intervals, and
    /// decomposes each interval into maximal-order buddy chunks via
    /// `split_to_chunks`.  This avoids flooding the buddy allocator with
    /// tiny order-0 / order-1 chunks.
    fn drain(&mut self, palloc: &mut BuddyPageAllocator<PAGE_SIZE_4K>, free_addr: usize) {
        let nr_to_drain = COUNT * 2 / 3; // additional entries to pop
        let blk_bytes = Self::block_bytes();

        // Collect: the freed block + popped blocks.
        let mut addrs = [0usize; DRAIN_BUF];
        let mut count = 0;
        addrs[count] = free_addr;
        count += 1;
        for _ in 0..nr_to_drain {
            if let Some(addr) = self.try_pop() {
                addrs[count] = addr;
                count += 1;
            } else {
                break;
            }
        }

        // Sort by address so physically-adjacent blocks can be merged.
        addrs[..count].sort_unstable();

        // Merge adjacent ranges, then decompose each into buddy chunks.
        let mut i = 0;
        while i < count {
            let start = addrs[i];
            let mut end = start + blk_bytes;
            // Extend the range while the next block starts right where
            // the current range ends.
            while i + 1 < count && addrs[i + 1] == end {
                i += 1;
                end = addrs[i] + blk_bytes;
            }
            i += 1;
            // Decompose [start, end) into valid buddy chunks.
            let range_bytes = end - start;
            for (chunk_addr, order) in split_to_chunks::<PAGE_SIZE_4K>(start, range_bytes) {
                palloc.deallocate_pages(chunk_addr, 1 << order);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// PerCpuPageSet — aggregate for all four block sizes
// ---------------------------------------------------------------------------

/// Per-CPU page set: collects per-order caches for 1–4 page blocks.
///
/// This is analogous to Linux's `struct per_cpu_pageset`.
#[derive(Clone, Copy)]
struct PerCpuPageSet {
    cache1: PerCpuPages<1, C1>,
    cache2: PerCpuPages<2, C2>,
    cache3: PerCpuPages<3, C3>,
    cache4: PerCpuPages<4, C4>,
}

impl PerCpuPageSet {
    const fn new() -> Self {
        Self {
            cache1: PerCpuPages::new(),
            cache2: PerCpuPages::new(),
            cache3: PerCpuPages::new(),
            cache4: PerCpuPages::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Per-CPU storage
// ---------------------------------------------------------------------------

#[percpu::def_percpu]
static PCP_PAGESET: PerCpuPageSet = PerCpuPageSet::new();

// ---------------------------------------------------------------------------
// Public module API
// ---------------------------------------------------------------------------

/// Try to pop a block of `num_pages` from the per-CPU cache.
///
/// Returns `None` if the corresponding cache is empty.
/// `num_pages` must be in 1..=4; other values always return `None`.
///
/// # Safety
///
/// Must be called with local IRQs disabled.
#[inline]
pub(crate) fn try_alloc(num_pages: usize) -> Option<usize> {
    PCP_PAGESET.with_current(|pageset| match num_pages {
        1 => pageset.cache1.try_pop(),
        2 => pageset.cache2.try_pop(),
        3 => pageset.cache3.try_pop(),
        4 => pageset.cache4.try_pop(),
        _ => None,
    })
}

/// Try to push a freed block of `num_pages` into the per-CPU cache.
///
/// Returns `true` if the block was cached, `false` if the cache is full
/// and the caller should fall back to the global buddy allocator.
/// `num_pages` must be in 1..=4.
///
/// # Safety
///
/// Must be called with local IRQs disabled.
#[inline]
pub(crate) fn try_free(num_pages: usize, addr: usize) -> bool {
    PCP_PAGESET.with_current(|pageset| match num_pages {
        1 => pageset.cache1.try_push(addr).is_ok(),
        2 => pageset.cache2.try_push(addr).is_ok(),
        3 => pageset.cache3.try_push(addr).is_ok(),
        4 => pageset.cache4.try_push(addr).is_ok(),
        _ => false,
    })
}

/// Bulk-fill the cache for `num_pages`-size blocks from the buddy allocator.
///
/// Called when [`try_alloc`] returned `None`.  Allocates up to a batch of
/// blocks from the global pool, fills the cache with whatever it could
/// obtain, and returns one block.  Returns `Err` when the batch obtained
/// nothing.
///
/// Must be called while holding the global buddy allocator lock (IRQs disabled).
pub(crate) fn fill_cache(
    num_pages: usize,
    palloc: &mut BuddyPageAllocator<PAGE_SIZE_4K>,
) -> AllocResult<usize> {
    PCP_PAGESET.with_current(|pageset| match num_pages {
        1 => pageset.cache1.alloc(palloc),
        2 => pageset.cache2.alloc(palloc),
        3 => pageset.cache3.alloc(palloc),
        4 => pageset.cache4.alloc(palloc),
        _ => unreachable!("fill_cache: num_pages={} not in 1..=4", num_pages),
    })
}

/// Bulk-drain the cache for `num_pages`-size blocks to the buddy allocator.
///
/// Called when [`try_free`] returned `false`.  Pops up to `COUNT * 2/3`
/// additional blocks from the cache and returns them all to the global pool.
///
/// Must be called while holding the global buddy allocator lock (IRQs disabled).
pub(crate) fn drain_cache(
    num_pages: usize,
    palloc: &mut BuddyPageAllocator<PAGE_SIZE_4K>,
    addr: usize,
) {
    PCP_PAGESET.with_current(|pageset| match num_pages {
        1 => pageset.cache1.dealloc(palloc, addr),
        2 => pageset.cache2.dealloc(palloc, addr),
        3 => pageset.cache3.dealloc(palloc, addr),
        4 => pageset.cache4.dealloc(palloc, addr),
        _ => unreachable!("drain_cache: num_pages={} not in 1..=4", num_pages),
    });
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(unittest)]
#[allow(missing_docs)]
mod tests {
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
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-CPU page cache (PCP) for fast single-page allocation.
//!
//! This module implements a per-CPU fixed-size cache of single pages,
//! designed to reduce contention on the global buddy allocator lock.
//! Pages are bulk-filled from the global pool when the cache is empty
//! and bulk-drained when it's full.
//!
//! # Safety
//!
//! All public functions must be called with local IRQs disabled to prevent
//! reentrancy from interrupt handlers on the same CPU.

use alloc_engine::{AllocResult, BuddyPageAllocator, PageAllocator};
use memaddr::PAGE_SIZE_4K;

/// Maximum number of cached pages per CPU.
const CACHE_MAX: usize = 16;

/// Number of pages to bulk-fill or bulk-drain at once (half of CACHE_MAX).
const CACHE_BATCH: usize = CACHE_MAX / 2;

/// Per-CPU page cache.
///
/// Stores addresses of single pages that have been allocated from the global
/// buddy allocator but not yet used, or freed by the owning CPU but not yet
/// returned to the global pool.
#[percpu::def_percpu]
static PCP_CACHE: PerCpuPageCache = PerCpuPageCache::new();

#[derive(Clone, Copy)]
struct PerCpuPageCache {
    addrs: [usize; CACHE_MAX],
    size: u8,
}

impl PerCpuPageCache {
    const fn new() -> Self {
        Self {
            addrs: [0; CACHE_MAX],
            size: 0,
        }
    }

    /// Pop a cached page address. Returns `None` if empty.
    fn try_pop(&mut self) -> Option<usize> {
        if self.size == 0 {
            return None;
        }
        self.size -= 1;
        Some(self.addrs[self.size as usize])
    }

    /// Push a page address into the cache. Returns `Err(addr)` if full.
    fn try_push(&mut self, addr: usize) -> Result<(), usize> {
        if self.size as usize >= CACHE_MAX {
            return Err(addr);
        }
        self.addrs[self.size as usize] = addr;
        self.size += 1;
        Ok(())
    }

    /// Current number of cached pages.
    #[allow(dead_code)]
    fn len(&self) -> usize {
        self.size as usize
    }
}

/// Try to pop a page from the per-CPU cache.
///
/// Returns the page address on success, or `None` if the cache is empty.
///
/// # Safety
///
/// Must be called with local IRQs disabled.
#[inline]
pub(crate) fn try_alloc() -> Option<usize> {
    PCP_CACHE.with_current(|cache| cache.try_pop())
}

/// Try to push a freed page into the per-CPU cache.
///
/// Returns `true` if the page was cached, `false` if the cache is full and
/// the caller should fall back to the global allocator.
///
/// # Safety
///
/// Must be called with local IRQs disabled.
#[inline]
pub(crate) fn try_free(addr: usize) -> bool {
    PCP_CACHE.with_current(|cache| cache.try_push(addr).is_ok())
}

/// Bulk-fill the per-CPU cache from the global buddy allocator.
///
/// Allocates `CACHE_BATCH` pages from `palloc`, pushes `CACHE_BATCH - 1`
/// into the cache, and returns the remaining page to the caller.
///
/// Must be called while holding the global allocator lock (IRQs disabled).
pub(crate) fn fill_cache(palloc: &mut BuddyPageAllocator<{ PAGE_SIZE_4K }>) -> AllocResult<usize> {
    // Pre-allocate the batch from the global pool. Track how many
    // succeed so we can roll back partial allocations on OOM.
    let mut addrs = [0usize; CACHE_BATCH];
    let mut allocated = 0;
    for i in 0..CACHE_BATCH {
        match palloc.allocate_pages(1, PAGE_SIZE_4K) {
            Ok(addr) => {
                addrs[i] = addr;
                allocated = i + 1;
            }
            Err(e) => {
                for addr in &addrs[..allocated] {
                    palloc.deallocate_pages(*addr, 1);
                }
                return Err(e);
            }
        }
    }
    // Push all but the last into the per-CPU cache.
    PCP_CACHE.with_current(|cache| {
        for addr in &addrs[..CACHE_BATCH - 1] {
            if cache.try_push(*addr).is_err() {
                // Defensive: cache was unexpectedly full; return page to pool.
                palloc.deallocate_pages(*addr, 1);
            }
        }
    });
    Ok(addrs[CACHE_BATCH - 1])
}

/// Bulk-drain pages from the per-CPU cache back to the global allocator.
///
/// Pushes `addr` plus up to `CACHE_BATCH` additional pages drained from the
/// cache, then deallocates them all to the global pool.
///
/// Must be called while holding the global allocator lock (IRQs disabled).
pub(crate) fn drain_cache(palloc: &mut BuddyPageAllocator<{ PAGE_SIZE_4K }>, addr: usize) {
    let mut batch = [0usize; CACHE_BATCH + 1];
    let mut count = 1;
    batch[0] = addr;

    // Pop up to CACHE_BATCH pages from cache.
    PCP_CACHE.with_current(|cache| {
        for _ in 0..CACHE_BATCH {
            if let Some(a) = cache.try_pop() {
                batch[count] = a;
                count += 1;
            } else {
                break;
            }
        }
    });

    // Return all collected pages to the global allocator.
    for addr in batch.iter().take(count) {
        palloc.deallocate_pages(*addr, 1);
    }
}

#[cfg(unittest)]
#[allow(missing_docs)]
mod tests {
    use unittest::def_test;

    use super::{CACHE_BATCH, CACHE_MAX, PerCpuPageCache};

    #[def_test]
    fn test_cache_new_empty() {
        let cache = PerCpuPageCache::new();
        assert_eq!(cache.len(), 0);
    }

    #[def_test]
    fn test_push_pop_single_page() {
        let mut cache = PerCpuPageCache::new();
        let addr = 0x1000;
        assert!(cache.try_push(addr).is_ok());
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.try_pop(), Some(addr));
        assert_eq!(cache.len(), 0);
    }

    #[def_test]
    fn test_pop_empty_returns_none() {
        let mut cache = PerCpuPageCache::new();
        assert_eq!(cache.try_pop(), None);
    }

    #[def_test]
    fn test_push_full_returns_err() {
        let mut cache = PerCpuPageCache::new();
        for i in 0..CACHE_MAX {
            let addr = 0x1000 * (i + 1);
            assert!(cache.try_push(addr).is_ok());
        }
        assert_eq!(cache.len(), CACHE_MAX);
        // The next push should fail.
        assert_eq!(cache.try_push(0xFFFF_0000), Err(0xFFFF_0000));
    }

    #[def_test]
    fn test_push_pop_fifo_order() {
        let mut cache = PerCpuPageCache::new();
        cache.try_push(0x1000).unwrap();
        cache.try_push(0x2000).unwrap();
        cache.try_push(0x3000).unwrap();
        // LIFO order (stack).
        assert_eq!(cache.try_pop(), Some(0x3000));
        assert_eq!(cache.try_pop(), Some(0x2000));
        assert_eq!(cache.try_pop(), Some(0x1000));
    }

    #[def_test]
    fn test_drain_partial() {
        let mut cache = PerCpuPageCache::new();
        for i in 0..CACHE_BATCH {
            cache.try_push(0x1000 * (i + 1)).unwrap();
        }
        assert_eq!(cache.len(), CACHE_BATCH);
        // Pop CACHE_BATCH items.
        for _ in 0..CACHE_BATCH {
            assert!(cache.try_pop().is_some());
        }
        assert_eq!(cache.len(), 0);
    }

    #[def_test]
    fn test_cache_constants() {
        assert!(CACHE_MAX >= 4);
        assert!(CACHE_BATCH >= 2);
        assert_eq!(CACHE_BATCH * 2, CACHE_MAX);
    }
}

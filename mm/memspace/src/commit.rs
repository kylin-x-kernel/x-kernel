// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Default Linux overcommit admission and private mapping commitments.

use core::sync::atomic::{AtomicUsize, Ordering};

use kerrno::{KError, KResult};
use memaddr::PAGE_SIZE_4K;

// This counter is independent of PTE residency and never publishes memory.
static COMMITTED_PAGES: AtomicUsize = AtomicUsize::new(0);

/// Checks a new private memory commitment against managed RAM.
///
/// Mirrors Linux OVERCOMMIT_GUESS: compare this request, rather than the
/// accumulated commitment or currently free memory, with RAM plus swap.
/// X-Kernel has no swap, so managed RAM is the entire admission capacity.
/// Passing admission preserves lazy allocation and does not guarantee that a
/// later page fault can allocate backing memory.
///
/// # Errors
///
/// Returns `KError::NoMemory` when the new commitment exceeds managed RAM.
pub fn check_memory_commitment(size_bytes: usize) -> KResult {
    if size_bytes.div_ceil(PAGE_SIZE_4K) > kalloc::global_allocator().total_pages() {
        return Err(KError::NoMemory);
    }
    Ok(())
}

/// Returns private mapping commitments in base pages, including COW promises.
///
/// This is Linux's `Committed_AS` accounting, not physical memory consumption
/// or a global hard limit. Shared-memory backing retains its object quotas.
pub fn committed_pages() -> usize {
    COMMITTED_PAGES.load(Ordering::Relaxed)
}

/// Rolls back a charge until VMA metadata takes ownership of it.
#[derive(Default)]
pub(crate) struct CommitCharge {
    pages: usize,
}

impl CommitCharge {
    pub(crate) fn new(size_bytes: usize) -> KResult<Self> {
        check_memory_commitment(size_bytes)?;
        let pages = size_bytes / PAGE_SIZE_4K;
        COMMITTED_PAGES
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(pages)
            })
            .map_err(|_| KError::NoMemory)?;
        Ok(Self { pages })
    }

    pub(crate) fn commit(mut self) {
        self.pages = 0;
    }
}

impl Drop for CommitCharge {
    fn drop(&mut self) {
        uncharge_pages(self.pages);
    }
}

pub(crate) fn uncharge(size_bytes: usize) {
    uncharge_pages(size_bytes / PAGE_SIZE_4K);
}

fn uncharge_pages(pages: usize) {
    if pages != 0 {
        let previous = COMMITTED_PAGES.fetch_sub(pages, Ordering::Relaxed);
        debug_assert!(previous >= pages);
    }
}

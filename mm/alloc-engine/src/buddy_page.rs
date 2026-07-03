// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Buddy allocation in page-granularity.

use crate::{AllocResult, BaseAllocator, PageAllocator, buddy_alloc::BuddyAllocator};

/// A page-granularity memory allocator based on the buddy allocator.
pub struct BuddyPageAllocator<const PAGE_SIZE: usize> {
    inner: BuddyAllocator<PAGE_SIZE>,
}

impl<const PAGE_SIZE: usize> BuddyPageAllocator<PAGE_SIZE> {
    /// Creates a new empty `BuddyPageAllocator`.
    pub const fn new() -> Self {
        Self {
            inner: BuddyAllocator::new(),
        }
    }
}

impl<const PAGE_SIZE: usize> BaseAllocator for BuddyPageAllocator<PAGE_SIZE> {
    fn init_region(&mut self, start: usize, size: usize) {
        self.inner
            .init_region(start, size)
            .expect("buddy allocator init failed");
    }

    fn add_region(&mut self, start: usize, size: usize) -> AllocResult {
        self.inner.add_region(start, size)
    }
}

impl<const PAGE_SIZE: usize> PageAllocator for BuddyPageAllocator<PAGE_SIZE> {
    const PAGE_SIZE: usize = PAGE_SIZE;

    fn allocate_pages(&mut self, num_pages: usize, align_pow2: usize) -> AllocResult<usize> {
        self.inner.allocate_pages(num_pages, align_pow2)
    }

    fn deallocate_pages(&mut self, base: usize, num_pages: usize) {
        self.inner.deallocate_pages(base, num_pages);
    }

    fn allocate_pages_at(
        &mut self,
        base: usize,
        num_pages: usize,
        align_pow2: usize,
    ) -> AllocResult<usize> {
        self.inner.allocate_pages_at(base, num_pages, align_pow2)
    }

    fn total_pages(&self) -> usize {
        self.inner.total_pages()
    }

    fn used_pages(&self) -> usize {
        self.inner.used_pages()
    }

    fn available_pages(&self) -> usize {
        self.inner.available_pages()
    }
}

impl<const PAGE_SIZE: usize> Default for BuddyPageAllocator<PAGE_SIZE> {
    fn default() -> Self {
        Self::new()
    }
}

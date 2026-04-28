// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Buddy allocation in page-granularity.

use buddy_slab_allocator::{AllocError as BuddyAllocError, BuddyAllocator};

use crate::{AllocError, AllocResult, BaseAllocator, PageAllocator};

/// A page-granularity memory allocator based on the [buddy allocator].
///
/// [slab allocator]: ../slab_allocator/index.html
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

    /// Allocate pages whose physical address is below 4 GiB (DMA32 zone).
    pub fn allocate_pages_lowmem(
        &mut self,
        num_pages: usize,
        align_pow2: usize,
    ) -> AllocResult<usize> {
        self.inner
            .alloc_pages_lowmem(num_pages, align_pow2)
            .map_err(map_alloc_error)
    }
}

fn map_alloc_error(err: BuddyAllocError) -> AllocError {
    match err {
        BuddyAllocError::NoMemory => AllocError::NoMemory,
        BuddyAllocError::MemoryOverlap => AllocError::MemoryOverlap,
        BuddyAllocError::NotAllocated => AllocError::NotAllocated,
        _ => AllocError::InvalidInput,
    }
}

impl<const PAGE_SIZE: usize> BaseAllocator for BuddyPageAllocator<PAGE_SIZE> {
    fn init_region(&mut self, start: usize, size: usize) {
        let region = unsafe { core::slice::from_raw_parts_mut(start as *mut u8, size) };
        unsafe {
            self.inner
                .init(region)
                .expect("buddy allocator init failed");
        }
    }

    fn add_region(&mut self, start: usize, size: usize) -> AllocResult {
        let region = unsafe { core::slice::from_raw_parts_mut(start as *mut u8, size) };
        unsafe { self.inner.add_region(region).map_err(map_alloc_error) }
    }
}

impl<const PAGE_SIZE: usize> PageAllocator for BuddyPageAllocator<PAGE_SIZE> {
    const PAGE_SIZE: usize = PAGE_SIZE;

    fn allocate_pages(&mut self, num_pages: usize, align_pow2: usize) -> AllocResult<usize> {
        self.inner
            .alloc_pages(num_pages, align_pow2)
            .map_err(map_alloc_error)
    }

    fn deallocate_pages(&mut self, base: usize, num_pages: usize) {
        self.inner.dealloc_pages(base, num_pages);
    }

    fn allocate_pages_at(
        &mut self,
        base: usize,
        _num_pages: usize,
        _align_pow2: usize,
    ) -> AllocResult<usize> {
        // v0.4.0 BuddyAllocator does not support alloc_pages_at
        let _ = base;
        Err(AllocError::InvalidInput)
    }

    fn total_pages(&self) -> usize {
        self.inner.total_pages()
    }

    fn used_pages(&self) -> usize {
        self.inner.total_pages() - self.inner.free_pages()
    }

    fn available_pages(&self) -> usize {
        self.inner.free_pages()
    }
}

impl<const PAGE_SIZE: usize> Default for BuddyPageAllocator<PAGE_SIZE> {
    fn default() -> Self {
        Self::new()
    }
}

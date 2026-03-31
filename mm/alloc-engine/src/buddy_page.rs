// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Buddy allocation in page-granularity.

use buddy_slab_allocator::{
    AddrTranslator, AllocError as BuddyAllocError, BaseAllocator as BuddyBaseAllocator,
    CompositePageAllocator, PageAllocator as BuddyPageAllocatorTrait,
};

use crate::{AllocError, AllocResult, BaseAllocator, PageAllocator};

/// A page-granularity memory allocator based on the [buddy allocator].
///
/// [slab allocator]: ../slab_allocator/index.html
pub struct BuddyPageAllocator<const PAGE_SIZE: usize> {
    inner: CompositePageAllocator<PAGE_SIZE>,
}

impl<const PAGE_SIZE: usize> BuddyPageAllocator<PAGE_SIZE> {
    /// Creates a new empty `BuddyPageAllocator`.
    pub const fn new() -> Self {
        Self {
            inner: CompositePageAllocator::new(),
        }
    }

    pub fn set_addr_translator(&mut self, translator: &'static dyn AddrTranslator) {
        self.inner.set_addr_translator(translator);
    }

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
        BuddyBaseAllocator::init(&mut self.inner, start, size);
    }

    fn add_region(&mut self, start: usize, size: usize) -> AllocResult {
        BuddyBaseAllocator::add_memory(&mut self.inner, start, size).map_err(map_alloc_error)
    }
}

impl<const PAGE_SIZE: usize> PageAllocator for BuddyPageAllocator<PAGE_SIZE> {
    const PAGE_SIZE: usize = PAGE_SIZE;

    fn allocate_pages(&mut self, num_pages: usize, align_pow2: usize) -> AllocResult<usize> {
        BuddyPageAllocatorTrait::alloc_pages(&mut self.inner, num_pages, align_pow2)
            .map_err(map_alloc_error)
    }

    fn deallocate_pages(&mut self, base: usize, num_pages: usize) {
        BuddyPageAllocatorTrait::dealloc_pages(&mut self.inner, base, num_pages);
    }

    fn allocate_pages_at(
        &mut self,
        base: usize,
        num_pages: usize,
        align_pow2: usize,
    ) -> AllocResult<usize> {
        BuddyPageAllocatorTrait::alloc_pages_at(&mut self.inner, base, num_pages, align_pow2)
            .map_err(map_alloc_error)
    }

    fn total_pages(&self) -> usize {
        BuddyPageAllocatorTrait::total_pages(&self.inner)
    }

    fn used_pages(&self) -> usize {
        BuddyPageAllocatorTrait::used_pages(&self.inner)
    }

    fn available_pages(&self) -> usize {
        BuddyPageAllocatorTrait::available_pages(&self.inner)
    }
}

impl<const PAGE_SIZE: usize> Default for BuddyPageAllocator<PAGE_SIZE> {
    fn default() -> Self {
        Self::new()
    }
}

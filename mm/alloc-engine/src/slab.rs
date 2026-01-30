// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Slab-based memory allocation.
//!
//! This module wraps `slab_allocator::Heap` for byte-granularity allocation.

use core::{alloc::Layout, ptr::NonNull};

use slab_allocator::Heap;

use super::{AllocError, AllocResult, BaseAllocator, ByteAllocator};

/// A byte-granularity memory allocator based on the [slab allocator].
///
/// [slab allocator]: ../slab_allocator/index.html
pub struct SlabByteAllocator {
    heap: Option<Heap>,
}

impl SlabByteAllocator {
    /// Creates a new empty `SlabByteAllocator`.
    pub const fn new() -> Self {
        Self { heap: None }
    }

    fn inner_mut(&mut self) -> &mut Heap {
        self.heap.as_mut().unwrap()
    }

    fn inner(&self) -> &Heap {
        self.heap.as_ref().unwrap()
    }
}

impl Default for SlabByteAllocator {
    fn default() -> Self {
        Self::new()
    }
}

impl BaseAllocator for SlabByteAllocator {
    fn init_region(&mut self, start: usize, size: usize) {
        self.heap = unsafe { Some(Heap::new(start, size)) };
    }

    fn add_region(&mut self, start: usize, size: usize) -> AllocResult {
        unsafe {
            self.inner_mut().add_memory(start, size);
        }
        Ok(())
    }
}

impl ByteAllocator for SlabByteAllocator {
    fn allocate(&mut self, layout: Layout) -> AllocResult<NonNull<u8>> {
        self.inner_mut()
            .allocate(layout)
            .map(|addr| unsafe { NonNull::new_unchecked(addr as *mut u8) })
            .map_err(|_| AllocError::NoMemory)
    }

    fn deallocate(&mut self, ptr: NonNull<u8>, layout: Layout) {
        unsafe { self.inner_mut().deallocate(ptr.as_ptr() as usize, layout) }
    }

    fn total_bytes(&self) -> usize {
        self.inner().total_bytes()
    }

    fn used_bytes(&self) -> usize {
        self.inner().used_bytes()
    }

    fn available_bytes(&self) -> usize {
        self.inner().available_bytes()
    }
}

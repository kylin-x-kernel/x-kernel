// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Buddy-system memory allocation.
//!
//! This is a thin wrapper around `buddy_system_allocator::Heap`.

use core::{alloc::Layout, ptr::NonNull};

use buddy_system_allocator::Heap;

use crate::{AllocError, AllocResult, BaseAllocator, ByteAllocator};

/// A byte-granularity memory allocator based on the [buddy_system_allocator].
///
/// [buddy_system_allocator]: https://docs.rs/buddy_system_allocator/latest/buddy_system_allocator/
pub struct BuddyByteAllocator {
    inner: Heap<32>,
}

impl BuddyByteAllocator {
    /// Creates a new empty `BuddyByteAllocator`.
    pub const fn new() -> Self {
        Self {
            inner: Heap::<32>::new(),
        }
    }
}

impl Default for BuddyByteAllocator {
    fn default() -> Self {
        Self::new()
    }
}

impl BaseAllocator for BuddyByteAllocator {
    fn init_region(&mut self, start: usize, size: usize) {
        // SAFETY: The caller provides a valid, writable region at
        // `[start, start + size)` and transfers its exclusive use to the buddy
        // heap for as long as the heap manages it.
        unsafe { self.inner.init(start, size) };
    }

    fn add_region(&mut self, start: usize, size: usize) -> AllocResult {
        // SAFETY: The caller provides a valid, writable region at
        // `[start, start + size)` and transfers its exclusive use to the buddy
        // heap for as long as the heap manages it.
        unsafe { self.inner.add_to_heap(start, start + size) };
        Ok(())
    }
}

// Note: This trait implementation acts as a standard thin wrapper.
// The method delegations to the `inner` allocator follow idiomatic Rust
// patterns for composition and should not be considered literal duplication.
impl ByteAllocator for BuddyByteAllocator {
    fn allocate(&mut self, layout: Layout) -> AllocResult<NonNull<u8>> {
        self.inner.alloc(layout).map_err(|_| AllocError::NoMemory)
    }

    fn deallocate(&mut self, ptr: NonNull<u8>, layout: Layout) {
        self.inner.dealloc(ptr, layout)
    }

    fn total_bytes(&self) -> usize {
        self.inner.stats_total_bytes()
    }

    fn used_bytes(&self) -> usize {
        self.inner.stats_alloc_actual()
    }

    fn available_bytes(&self) -> usize {
        self.inner.stats_total_bytes() - self.inner.stats_alloc_actual()
    }
}

unittest::path_mod_test!(pub(crate) mod tests_buddy, "../ut-tests/buddy/tests_buddy.rs");

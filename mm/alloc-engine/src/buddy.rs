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
        unsafe { self.inner.init(start, size) };
    }

    fn add_region(&mut self, start: usize, size: usize) -> AllocResult {
        unsafe { self.inner.add_to_heap(start, start + size) };
        Ok(())
    }
}

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

#[cfg(all(feature = "alloc_engine_test", feature = "buddy"))]
#[allow(missing_docs)]
pub mod tests_buddy {
    use core::alloc::Layout;

    use unittest::{
        test_fn, test_framework::TestDescriptor, test_framework_basic::TestResult, tests_name,
    };

    use super::BuddyByteAllocator;
    use crate::{BaseAllocator, ByteAllocator};

    test_fn! {
        using TestResult;

        fn test_buddy_allocate_deallocate() {
            let mut alloc = BuddyByteAllocator::new();
            let mut heap = alloc::vec![0u8; 4096].into_boxed_slice();
            let base = heap.as_mut_ptr() as usize;
            let size = heap.len();
            alloc.init_region(base, size);
            let layout = Layout::from_size_align(64, 8).unwrap();
            let ptr = alloc.allocate(layout).unwrap();
            assert!(alloc.used_bytes() >= 64);
            alloc.deallocate(ptr, layout);
            assert!(alloc.used_bytes() <= alloc.total_bytes());
        }
    }

    test_fn! {
        using TestResult;

        fn test_buddy_available_bytes() {
            let mut alloc = BuddyByteAllocator::new();
            let mut heap = alloc::vec![0u8; 4096].into_boxed_slice();
            let base = heap.as_mut_ptr() as usize;
            let size = heap.len();
            alloc.init_region(base, size);
            assert_eq!(alloc.total_bytes(), alloc.available_bytes());
        }
    }

    tests_name! {
        TEST_BUDDY;
        test_buddy_allocate_deallocate,
        test_buddy_available_bytes,
    }
}

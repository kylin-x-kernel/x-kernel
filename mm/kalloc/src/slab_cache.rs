// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-CPU cache for small slab objects.
//!
//! Each size class keeps an intrusive list of objects freed on the current
//! CPU. A cache hit only touches that list. An empty cache is refilled in a
//! batch from the globally locked byte allocator, and an overfull cache is
//! drained back to it in a batch.
//!
//! Cached objects remain allocated from the central slab heap. This makes a
//! cross-CPU free safe to cache on the freeing CPU and avoids remote-owner
//! routing, at the cost of bounded unreclaimable cache residency.

use core::{
    alloc::Layout,
    mem::{align_of, size_of},
    ptr::NonNull,
};

use alloc_engine::{AllocError, AllocResult, ByteAllocator, SizeClass};
use memaddr::PAGE_SIZE_4K;

/// Target object bytes in one refill or drain batch.
const OBJECT_CACHE_TARGET_BYTES: usize = 2 * PAGE_SIZE_4K;

/// Maximum objects transferred while holding the central allocator lock.
const OBJECT_CACHE_BATCH_MAX_OBJECTS: usize = 32;

// Every supported size class starts at 8 bytes and is at least 8-byte
// aligned, so its first word can hold the intrusive link on all build targets.
const _: () = {
    assert!(size_of::<Option<NonNull<u8>>>() <= 8);
    assert!(align_of::<Option<NonNull<u8>>>() <= 8);
};

/// Intrusive free-object list for one slab size class.
///
/// The first machine word of every cached object stores the next address.
struct ObjectCache<const OBJECT_SIZE: usize> {
    head: Option<NonNull<u8>>,
    len: usize,
}

impl<const OBJECT_SIZE: usize> ObjectCache<OBJECT_SIZE> {
    const fn new() -> Self {
        Self { head: None, len: 0 }
    }

    const fn max_objects() -> usize {
        // Keeping the high watermark one slot below two full batches lets a
        // full-cache drain include the newly freed object in one batch.
        Self::target_objects() * 2 - 1
    }

    const fn target_objects() -> usize {
        let target_by_bytes = OBJECT_CACHE_TARGET_BYTES / OBJECT_SIZE;
        if target_by_bytes < OBJECT_CACHE_BATCH_MAX_OBJECTS {
            target_by_bytes
        } else {
            OBJECT_CACHE_BATCH_MAX_OBJECTS
        }
    }

    fn canonical_layout() -> Layout {
        Layout::from_size_align(OBJECT_SIZE, OBJECT_SIZE)
            .expect("slab object size must be a power of two")
    }

    fn pop(&mut self) -> Option<NonNull<u8>> {
        let Some(object) = self.head else {
            debug_assert_eq!(self.len, 0);
            return None;
        };
        // SAFETY: `head` was installed only by `push_unchecked` from a live,
        // class-aligned cached object. Its first word contains the next link.
        self.head = unsafe { object.cast::<Option<NonNull<u8>>>().as_ptr().read() };
        self.len -= 1;
        Some(object)
    }

    /// Push an object without checking the high watermark.
    ///
    /// # Safety
    ///
    /// `object` must be an exclusively owned, writable object allocated with
    /// this cache's canonical layout. It must not already be in any cache.
    unsafe fn push_unchecked(&mut self, object: NonNull<u8>) {
        debug_assert!(OBJECT_SIZE >= size_of::<Option<NonNull<u8>>>());
        debug_assert!(self.len < Self::max_objects());

        // SAFETY: the caller gives this cache exclusive ownership of a live,
        // class-aligned object whose first machine word may be overwritten.
        unsafe {
            object
                .cast::<Option<NonNull<u8>>>()
                .as_ptr()
                .write(self.head)
        };
        self.head = Some(object);
        self.len += 1;
    }

    /// Try to cache one freed object.
    ///
    /// # Safety
    ///
    /// The pointer must satisfy [`Self::push_unchecked`]'s contract.
    unsafe fn try_push(&mut self, object: NonNull<u8>) -> Result<(), NonNull<u8>> {
        if self.len >= Self::max_objects() {
            return Err(object);
        }
        // SAFETY: forwarded from this function's caller after the capacity
        // check above established space in the cache.
        unsafe { self.push_unchecked(object) };
        Ok(())
    }

    /// Return one object, refilling an empty cache with one bounded batch.
    fn refill<A: ByteAllocator>(&mut self, allocator: &mut A) -> Option<NonNull<u8>> {
        // Keep this slow-path helper correct even if a caller probes before
        // entering its final IRQ-off, central-lock section.
        if let Some(object) = self.pop() {
            return Some(object);
        }

        let layout = Self::canonical_layout();
        let caller_object = allocator.allocate(layout).ok()?;
        for _ in 1..Self::target_objects() {
            let Ok(cached_object) = allocator.allocate(layout) else {
                break;
            };
            // SAFETY: the central allocator returned a fresh object with the
            // canonical layout, and the empty cache has room for one batch.
            unsafe { self.push_unchecked(cached_object) };
        }

        Some(caller_object)
    }

    /// Return an overfull cache to its target watermark.
    ///
    /// # Safety
    ///
    /// `object` must have been allocated by `allocator` with this cache's
    /// canonical layout and must no longer be accessible by its caller.
    unsafe fn drain<A: ByteAllocator>(&mut self, allocator: &mut A, object: NonNull<u8>) {
        let layout = Self::canonical_layout();
        allocator.deallocate(object, layout);
        while self.len > Self::target_objects() {
            let cached = self
                .pop()
                .expect("cache above target watermark must contain an object");
            allocator.deallocate(cached, layout);
        }
    }
}

/// All small-object caches belonging to one CPU.
struct PerCpuSlabCache {
    bytes8: ObjectCache<8>,
    bytes16: ObjectCache<16>,
    bytes32: ObjectCache<32>,
    bytes64: ObjectCache<64>,
    bytes128: ObjectCache<128>,
    bytes256: ObjectCache<256>,
    bytes512: ObjectCache<512>,
    bytes1024: ObjectCache<1024>,
    bytes2048: ObjectCache<2048>,
}

macro_rules! with_object_cache {
    ($set:expr, $size_class:expr, | $cache:ident | $body:expr) => {{
        match $size_class {
            SizeClass::Bytes8 => {
                let $cache = &mut ($set).bytes8;
                $body
            }
            SizeClass::Bytes16 => {
                let $cache = &mut ($set).bytes16;
                $body
            }
            SizeClass::Bytes32 => {
                let $cache = &mut ($set).bytes32;
                $body
            }
            SizeClass::Bytes64 => {
                let $cache = &mut ($set).bytes64;
                $body
            }
            SizeClass::Bytes128 => {
                let $cache = &mut ($set).bytes128;
                $body
            }
            SizeClass::Bytes256 => {
                let $cache = &mut ($set).bytes256;
                $body
            }
            SizeClass::Bytes512 => {
                let $cache = &mut ($set).bytes512;
                $body
            }
            SizeClass::Bytes1024 => {
                let $cache = &mut ($set).bytes1024;
                $body
            }
            SizeClass::Bytes2048 => {
                let $cache = &mut ($set).bytes2048;
                $body
            }
        }
    }};
}

impl PerCpuSlabCache {
    const fn new() -> Self {
        Self {
            bytes8: ObjectCache::new(),
            bytes16: ObjectCache::new(),
            bytes32: ObjectCache::new(),
            bytes64: ObjectCache::new(),
            bytes128: ObjectCache::new(),
            bytes256: ObjectCache::new(),
            bytes512: ObjectCache::new(),
            bytes1024: ObjectCache::new(),
            bytes2048: ObjectCache::new(),
        }
    }

    fn try_alloc(&mut self, size_class: SizeClass) -> Option<NonNull<u8>> {
        with_object_cache!(self, size_class, |cache| cache.pop())
    }

    /// # Safety
    ///
    /// `object` must be a live allocation made with the canonical layout for
    /// `size_class`, and the caller must have ended all access to it.
    unsafe fn try_free(
        &mut self,
        size_class: SizeClass,
        object: NonNull<u8>,
    ) -> Result<(), NonNull<u8>> {
        with_object_cache!(self, size_class, |cache| {
            // SAFETY: forwarded from this function's caller; the match keeps
            // the pointer in the corresponding size-class cache.
            unsafe { cache.try_push(object) }
        })
    }

    fn refill<A: ByteAllocator>(
        &mut self,
        size_class: SizeClass,
        allocator: &mut A,
    ) -> Option<NonNull<u8>> {
        with_object_cache!(self, size_class, |cache| cache.refill(allocator))
    }

    /// # Safety
    ///
    /// `object` must satisfy [`Self::try_free`]'s contract and `allocator`
    /// must be the central allocator that supplied this cache's objects.
    unsafe fn drain<A: ByteAllocator>(
        &mut self,
        size_class: SizeClass,
        allocator: &mut A,
        object: NonNull<u8>,
    ) {
        with_object_cache!(self, size_class, |cache| {
            // SAFETY: forwarded from this function's caller; the match keeps
            // the object and canonical layout in the same size class.
            unsafe { cache.drain(allocator, object) }
        });
    }
}

#[percpu::def_percpu]
static PERCPU_SLAB_CACHE: PerCpuSlabCache = PerCpuSlabCache::new();

/// Pop an object from the current CPU cache.
///
/// # Safety
///
/// Local IRQs must remain disabled for the entire call so an interrupt cannot
/// obtain an aliasing mutable reference to this CPU's cache.
#[inline]
pub(crate) unsafe fn try_alloc(size_class: SizeClass) -> Option<NonNull<u8>> {
    PERCPU_SLAB_CACHE.with_current(|cache| cache.try_alloc(size_class))
}

/// Cache a freed object on the current CPU.
///
/// # Safety
///
/// Local IRQs must remain disabled for the entire call. `object` must be a
/// live allocation made with the canonical layout for `size_class`, and the
/// caller must have ended all access to it.
#[inline]
pub(crate) unsafe fn try_free(size_class: SizeClass, object: NonNull<u8>) -> bool {
    PERCPU_SLAB_CACHE.with_current(|cache| {
        // SAFETY: forwarded from this function's caller.
        unsafe { cache.try_free(size_class, object) }.is_ok()
    })
}

/// Refill the current CPU cache while holding the central allocator lock.
///
/// # Safety
///
/// Local IRQs must remain disabled for the entire call. `allocator` must be
/// the central byte allocator used for all objects in these caches.
pub(crate) unsafe fn refill_cache<A: ByteAllocator>(
    size_class: SizeClass,
    allocator: &mut A,
) -> AllocResult<NonNull<u8>> {
    PERCPU_SLAB_CACHE
        .with_current(|cache| cache.refill(size_class, allocator))
        .ok_or(AllocError::NoMemory)
}

/// Drain an overfull current-CPU cache while holding the central lock.
///
/// # Safety
///
/// Local IRQs must remain disabled for the entire call. `object` must satisfy
/// [`try_free`]'s pointer contract, and `allocator` must be the central byte
/// allocator that supplied every cached object.
pub(crate) unsafe fn drain_cache<A: ByteAllocator>(
    size_class: SizeClass,
    allocator: &mut A,
    object: NonNull<u8>,
) {
    PERCPU_SLAB_CACHE.with_current(|cache| {
        // SAFETY: forwarded from this function's caller.
        unsafe { cache.drain(size_class, allocator, object) }
    });
}

#[cfg(unittest)]
#[allow(missing_docs)]
mod tests {
    use alloc::vec::Vec;

    use alloc_engine::{BaseAllocator, ByteAllocator};
    use unittest::def_test;

    use super::*;

    struct FakeAllocator {
        storage: Vec<usize>,
        next: usize,
        deallocated: usize,
    }

    impl FakeAllocator {
        fn with_slots(slots: usize) -> Self {
            Self {
                storage: alloc::vec![0; slots],
                next: 0,
                deallocated: 0,
            }
        }
    }

    impl BaseAllocator for FakeAllocator {
        fn init_region(&mut self, _base: usize, _size: usize) {}

        fn add_region(&mut self, _base: usize, _size: usize) -> AllocResult {
            Ok(())
        }
    }

    impl ByteAllocator for FakeAllocator {
        fn allocate(&mut self, layout: Layout) -> AllocResult<NonNull<u8>> {
            assert_eq!(layout, Layout::from_size_align(8, 8).unwrap());
            let Some(slot) = self.storage.get_mut(self.next) else {
                return Err(AllocError::NoMemory);
            };
            self.next += 1;
            Ok(NonNull::from(slot).cast())
        }

        fn deallocate(&mut self, _ptr: NonNull<u8>, layout: Layout) {
            assert_eq!(layout, Layout::from_size_align(8, 8).unwrap());
            self.deallocated += 1;
        }

        fn total_bytes(&self) -> usize {
            self.storage.len() * size_of::<usize>()
        }

        fn used_bytes(&self) -> usize {
            (self.next - self.deallocated) * size_of::<usize>()
        }

        fn available_bytes(&self) -> usize {
            self.total_bytes() - self.used_bytes()
        }
    }

    #[def_test]
    fn test_object_cache_lifo() {
        let mut slots = [0usize; 3];
        let mut cache = ObjectCache::<8>::new();
        let first = NonNull::from(&mut slots[0]).cast();
        let second = NonNull::from(&mut slots[1]).cast();

        // SAFETY: both stack slots are live, writable, aligned 8-byte objects
        // and are not accessible until they are popped below.
        unsafe {
            cache.try_push(first).unwrap();
            cache.try_push(second).unwrap();
        }
        assert_eq!(cache.pop(), Some(second));
        assert_eq!(cache.pop(), Some(first));
        assert_eq!(cache.pop(), None);
    }

    #[def_test]
    fn test_refill_reaches_target() {
        let target = ObjectCache::<8>::target_objects();
        let mut allocator = FakeAllocator::with_slots(target);
        let mut cache = ObjectCache::<8>::new();

        let object = cache.refill(&mut allocator).unwrap();
        assert_eq!(cache.len, target - 1);
        assert_eq!(allocator.next, target);
        assert_eq!(object, NonNull::from(&mut allocator.storage[0]).cast());
        assert!(allocator.next <= OBJECT_CACHE_BATCH_MAX_OBJECTS);
    }

    #[def_test]
    fn test_partial_refill_still_returns_object() {
        let mut allocator = FakeAllocator::with_slots(3);
        let mut cache = ObjectCache::<8>::new();

        assert!(cache.refill(&mut allocator).is_some());
        assert_eq!(cache.len, 2);
        assert_eq!(allocator.next, 3);
    }

    #[def_test]
    fn test_drain_returns_to_target() {
        let max = ObjectCache::<8>::max_objects();
        let target = ObjectCache::<8>::target_objects();
        let mut allocator = FakeAllocator::with_slots(max + 1);
        let mut objects = Vec::with_capacity(max + 1);
        for _ in 0..=max {
            objects.push(
                allocator
                    .allocate(Layout::from_size_align(8, 8).unwrap())
                    .unwrap(),
            );
        }

        let mut cache = ObjectCache::<8>::new();
        for &object in &objects[..max] {
            // SAFETY: each object is a distinct live allocation from the fake
            // central allocator and remains inaccessible while cached.
            unsafe { cache.try_push(object).unwrap() };
        }
        assert_eq!(cache.len, max);

        // SAFETY: the last object and all cached objects came from the same
        // allocator with the canonical 8-byte layout.
        unsafe { cache.drain(&mut allocator, objects[max]) };
        assert_eq!(cache.len, target);
        assert_eq!(allocator.deallocated, max - target + 1);
        assert!(allocator.deallocated <= OBJECT_CACHE_BATCH_MAX_OBJECTS);
    }

    #[def_test]
    fn test_object_cache_batch_and_capacity_bounds() {
        assert_eq!(ObjectCache::<8>::target_objects(), 32);
        assert_eq!(ObjectCache::<8>::max_objects(), 63);
        assert_eq!(ObjectCache::<256>::target_objects(), 32);
        assert_eq!(ObjectCache::<256>::max_objects(), 63);
        assert_eq!(ObjectCache::<512>::target_objects(), 16);
        assert_eq!(ObjectCache::<512>::max_objects(), 31);
        assert_eq!(ObjectCache::<1024>::target_objects(), 8);
        assert_eq!(ObjectCache::<1024>::max_objects(), 15);
        assert_eq!(ObjectCache::<2048>::target_objects(), 4);
        assert_eq!(ObjectCache::<2048>::max_objects(), 7);
    }
}

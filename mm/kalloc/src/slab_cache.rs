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
use kcpu_slot::cpu_slot_cell;
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

// SAFETY: `new()` contains only empty fixed-capacity object caches; no
// allocation or owned object is present in the copied template.
unsafe impl kcpu_slot::StaticSlotValue for PerCpuSlabCache {}

cpu_slot_cell! {
    static PERCPU_SLAB_CACHE: crate::slab_cache::PerCpuSlabCache =
        crate::slab_cache::PerCpuSlabCache::new();
}

/// Pop an object from the current CPU cache.
///
/// The owner keeps this operation pinned with local IRQs disabled.
#[inline]
pub(crate) fn try_alloc(
    owner: &mut crate::AllocLocalOwner,
    size_class: SizeClass,
) -> Option<NonNull<u8>> {
    let pin = owner.guard();
    // SAFETY: `owner` proves exclusive access to this CPU's slab cache.
    unsafe {
        PERCPU_SLAB_CACHE
            .get_mut_unchecked(pin)
            .try_alloc(size_class)
    }
}

/// Cache a freed object on the current CPU.
///
/// # Safety
///
/// `object` must be a live allocation made with the canonical layout for
/// `size_class`, and the caller must have ended all access to it. The owner
/// supplies the current-CPU exclusion requirement.
#[inline]
pub(crate) unsafe fn try_free(
    owner: &mut crate::AllocLocalOwner,
    size_class: SizeClass,
    object: NonNull<u8>,
) -> bool {
    let pin = owner.guard();
    // SAFETY: the pointer provenance contract is forwarded from this
    // function's caller; `owner` establishes exclusive local cache access.
    let cache = unsafe { PERCPU_SLAB_CACHE.get_mut_unchecked(pin) };
    // SAFETY: The caller transfers a live object of this size class and ends
    // all access to it before this cache stores its free-list link.
    unsafe { cache.try_free(size_class, object) }.is_ok()
}

/// Refill the current CPU cache while holding the central allocator lock.
///
/// # Safety
///
/// `allocator` must be the central byte allocator used for all objects in
/// these caches. The owner supplies the current-CPU exclusion requirement.
pub(crate) unsafe fn refill_cache<A: ByteAllocator>(
    owner: &mut crate::AllocLocalOwner,
    size_class: SizeClass,
    allocator: &mut A,
) -> AllocResult<NonNull<u8>> {
    let pin = owner.guard();
    // SAFETY: `owner` proves exclusive access to this CPU's slab cache.
    unsafe {
        PERCPU_SLAB_CACHE
            .get_mut_unchecked(pin)
            .refill(size_class, allocator)
    }
    .ok_or(AllocError::NoMemory)
}

/// Drain an overfull current-CPU cache while holding the central lock.
///
/// # Safety
///
/// `object` must satisfy [`try_free`]'s pointer contract, and `allocator` must
/// be the central byte allocator that supplied every cached object. The owner
/// supplies the current-CPU exclusion requirement.
pub(crate) unsafe fn drain_cache<A: ByteAllocator>(
    owner: &mut crate::AllocLocalOwner,
    size_class: SizeClass,
    allocator: &mut A,
    object: NonNull<u8>,
) {
    let pin = owner.guard();
    // SAFETY: the pointer and allocator provenance contracts are forwarded
    // from this function's caller; `owner` serializes local cache mutation.
    let cache = unsafe { PERCPU_SLAB_CACHE.get_mut_unchecked(pin) };
    // SAFETY: The caller supplies the originating central allocator and a
    // live, exclusively transferred object of the specified size class.
    unsafe { cache.drain(size_class, allocator, object) };
}

unittest::path_mod_test!(tests, "../ut-tests/slab_cache.rs");

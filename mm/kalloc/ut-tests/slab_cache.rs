// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

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
    unsafe { cache.try_push(first) }.unwrap();
    // SAFETY: The second stack slot is a distinct live aligned object;
    // the test does not access it again until it is popped.
    unsafe { cache.try_push(second) }.unwrap();
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

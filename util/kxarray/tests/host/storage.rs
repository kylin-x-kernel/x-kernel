// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Actual storage paths without a fabricated kernel ReadScope. These fixtures
//! exclude destructive mutations while holding unscoped raw borrows.
//! Kernel tests separately exercise admission through the real RCU host.

extern crate std;

use alloc::{collections::BTreeMap, vec::Vec};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    sync::Barrier,
    thread,
};

use super::*;

std::thread_local! {
    static ALLOCATION_COUNTS: Cell<Option<(usize, usize)>> = const { Cell::new(None) };
}

struct CountingAllocator;

#[global_allocator]
static HOST_ALLOCATOR: CountingAllocator = CountingAllocator;

// SAFETY: all storage operations delegate unchanged pointers/layouts to System.
// The const-initialized thread-local Cell only counts operations and does not
// allocate, unwind or share mutable state between concurrent test threads.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: GlobalAlloc's caller supplies a valid nonzero allocation layout.
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            let _ = ALLOCATION_COUNTS.try_with(|counts| {
                if let Some((allocated, freed)) = counts.get() {
                    counts.set(Some((allocated + 1, freed)));
                }
            });
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: the caller returns a live allocation with its original layout;
        // every allocation above came directly from System without adjustment.
        unsafe { System.dealloc(pointer, layout) };
        let _ = ALLOCATION_COUNTS.try_with(|counts| {
            if let Some((allocated, freed)) = counts.get() {
                counts.set(Some((allocated, freed + 1)));
            }
        });
    }
}

struct FailingAllocator<'a, P: EntryOwner> {
    array: &'a XArray<P>,
    remaining: usize,
}

impl<P: EntryOwner> Allocator for FailingAllocator<'_, P> {
    fn allocate<T>(&mut self, value: T) -> KResult<KBox<T>> {
        assert!(self.array.writers.try_lock().is_some());
        if self.remaining == 0 {
            return Err(KError::NoMemory);
        }
        self.remaining -= 1;
        KBox::new_nowait(value)
    }
}

#[test]
fn partial_node_preparation_releases_storage_without_publication() {
    for has_anchor in [false, true] {
        let reservations = if has_anchor {
            2 * (MAX_HEIGHT as usize - 1)
        } else {
            MAX_HEIGHT as usize
        };
        for remaining in 0..reservations {
            let array = XArray::<Arc<usize>>::new();
            if has_anchor {
                assert!(
                    // SAFETY: host fixture with primitive nonblocking Arc release.
                    unsafe { array.insert_with_flags(0, Arc::new(7), AllocFlags::NOWAIT) }.is_ok()
                );
            }
            let root = array.root.load(Ordering::Acquire);
            let offered = Arc::new(9);
            let mut allocator = FailingAllocator {
                array: &array,
                remaining,
            };
            ALLOCATION_COUNTS.with(|counts| counts.set(Some((0, 0))));
            // SAFETY: no destructive concurrency; allocations use the host backend.
            let result =
                unsafe { array.insert_with_allocator(usize::MAX, offered.clone(), &mut allocator) };
            let counts = ALLOCATION_COUNTS.with(|counts| counts.replace(None));
            assert_eq!(counts, Some((remaining, remaining)));
            let (error, returned) = result.unwrap_err();
            assert_eq!(error, KError::NoMemory);
            assert!(Arc::ptr_eq(&returned, &offered));
            assert_eq!(array.root.load(Ordering::Acquire), root);
            // SAFETY: the fixture owns the array and excludes destructive mutations.
            assert!(unsafe { array.lookup(usize::MAX) }.is_null());
            drop(returned);
            assert_eq!(Arc::strong_count(&offered), 1);
        }
    }
}

#[test]
fn invalid_permissions_return_owner_before_insertion() {
    let array = XArray::<Arc<usize>>::new();
    let offered = Arc::new(7);
    let invalid = AllocFlags::from_bits_retain(1 << 31);
    // SAFETY: host fixture; invalid flags reject before allocation or RCU access.
    let (error, returned) =
        unsafe { array.insert_with_flags(0, offered.clone(), invalid) }.unwrap_err();
    assert_eq!(error, KError::InvalidInput);
    assert!(Arc::ptr_eq(&returned, &offered));
    assert!(array.root.load(Ordering::Acquire).is_null());
}

#[test]
fn radix_boundaries_and_sparse_keys_match_a_reference_map() {
    let array = XArray::<KBox<usize>>::new();
    let mut model = BTreeMap::new();
    let boundaries = (1..usize::BITS).flat_map(|bit| {
        let boundary = 1_usize << bit;
        [boundary - 1, boundary, boundary + 1]
    });
    for key in [0, usize::MAX].into_iter().chain(boundaries) {
        let value = KBox::new_nowait(key).unwrap();
        let original = core::ptr::from_ref(&*value);
        // SAFETY: host-thread test with real SMP writer exclusion; primitive
        // ownership has no blocking destructor or kernel context dependency.
        let result =
            unsafe { array.insert_with_flags(key, value, kalloc_types::AllocFlags::NOWAIT) };
        if model.insert(key, key).is_some() {
            let (error, rejected) = result.unwrap_err();
            assert_eq!(error, kerrno::KError::AlreadyExists);
            assert_eq!(core::ptr::from_ref(&*rejected), original);
        } else {
            assert!(result.is_ok());
        }
        for expected in model.keys() {
            // SAFETY: this fixture performs only insertion, never reclamation.
            let raw = unsafe { array.lookup(*expected) }.cast::<usize>();
            // SAFETY: inserted KBox remains array-owned; no removal or mutable
            // payload access exists, and the array outlives this comparison.
            assert_eq!(unsafe { raw.as_ref() }, Some(expected));
        }
    }
    // SAFETY: no destructive mutations occur in this fixture.
    assert!(unsafe { array.lookup(42) }.is_null());
}

#[test]
fn concurrent_same_key_insertion_retains_exactly_one_strong_owner() {
    let array = XArray::<Arc<usize>>::new();
    let owners: Vec<_> = (0..4).map(Arc::new).collect();
    let barrier = Barrier::new(owners.len());
    thread::scope(|threads| {
        let handles: Vec<_> = owners
            .iter()
            .map(|owner| {
                let barrier = &barrier;
                let array = &array;
                threads.spawn(move || {
                    let offered = owner.clone();
                    barrier.wait();
                    // SAFETY: host task-equivalent context, primitive Arc release,
                    // real SMP lock; array remains alive through all thread joins.
                    match unsafe {
                        array.insert_with_flags(
                            usize::MAX,
                            offered,
                            kalloc_types::AllocFlags::NOWAIT,
                        )
                    } {
                        Ok(()) => true,
                        Err((error, returned)) => {
                            assert_eq!(error, kerrno::KError::AlreadyExists);
                            assert!(Arc::ptr_eq(&returned, owner));
                            false
                        }
                    }
                })
            })
            .collect();
        let wins = handles
            .into_iter()
            .map(|handle| usize::from(handle.join().unwrap()))
            .sum::<usize>();
        assert_eq!(wins, 1);
    });
    // SAFETY: all writers joined and this fixture never retires entries.
    let stored = unsafe { array.lookup(usize::MAX) }.cast::<usize>();
    for owner in &owners {
        assert_eq!(
            Arc::strong_count(owner),
            if Arc::as_ptr(owner) == stored { 2 } else { 1 }
        );
    }
    drop(array);
    assert!(owners.iter().all(|owner| Arc::strong_count(owner) == 1));
}

#[test]
fn root_growth_preserves_an_existing_payload_borrow() {
    let array = XArray::<KBox<u8>>::new();
    assert!(
        // SAFETY: primitive payload and host allocator, no kernel context needed.
        unsafe {
            array.insert_with_flags(
                0,
                KBox::new_nowait(7).unwrap(),
                kalloc_types::AllocFlags::NOWAIT,
            )
        }
        .is_ok()
    );
    // SAFETY: only insertions follow; array retains this path and owner.
    let raw = unsafe { array.lookup(0) }.cast::<u8>();
    // SAFETY: insertion-only storage retains the KBox through array destruction;
    // later growth modifies atomic links, never the borrowed payload.
    let borrowed = unsafe { &*raw };
    for bit in (6..usize::BITS).step_by(6) {
        assert!(
            // SAFETY: same host context and primitive ownership as above.
            unsafe {
                array.insert_with_flags(
                    1 << bit,
                    KBox::new_nowait(9).unwrap(),
                    kalloc_types::AllocFlags::NOWAIT,
                )
            }
            .is_ok()
        );
        assert_eq!(*borrowed, 7);
        // SAFETY: only insertion occurs, retaining all previously published nodes.
        assert_eq!(unsafe { array.lookup(0) }.cast::<u8>(), raw);
    }
    let units = XArray::<KBox<()>>::new();
    for key in [0, 64, usize::MAX] {
        assert!(
            // SAFETY: unit has no destructor and ZST KBox requires no deallocation.
            unsafe {
                units.insert_with_flags(
                    key,
                    KBox::new_nowait(()).unwrap(),
                    kalloc_types::AllocFlags::NOWAIT,
                )
            }
            .is_ok()
        );
        // SAFETY: insertion-only fixture retains the path and aligned ZST owner.
        let raw = unsafe { units.lookup(key) }.cast::<()>();
        // SAFETY: the array retains this aligned ZST owner throughout the borrow.
        assert_eq!(unsafe { raw.as_ref() }, Some(&()));
    }
}

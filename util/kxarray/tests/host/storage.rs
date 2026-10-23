// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Actual storage paths without a fabricated kernel ReadScope. These fixtures
//! exclude destructive mutations while holding unscoped raw borrows.
//! Kernel tests separately exercise admission through the real RCU host.

extern crate std;

use alloc::{collections::BTreeMap, vec::Vec};
use std::{sync::Barrier, thread};

use super::*;

#[test]
fn radix_boundaries_and_sparse_keys_match_a_reference_map() {
    let array = XArray::<Box<usize>>::new();
    let mut model = BTreeMap::new();
    let boundaries = (1..usize::BITS).flat_map(|bit| {
        let boundary = 1_usize << bit;
        [boundary - 1, boundary, boundary + 1]
    });
    for key in [0, usize::MAX].into_iter().chain(boundaries) {
        let value = Box::new(key);
        let original = core::ptr::from_ref(&*value);
        // SAFETY: host-thread test with real SMP writer exclusion; primitive
        // ownership has no blocking destructor or kernel context dependency.
        let result = unsafe { array.insert(key, value) };
        if model.insert(key, key).is_some() {
            let rejected = result.unwrap_err();
            assert_eq!(core::ptr::from_ref(&*rejected), original);
        } else {
            assert!(result.is_ok());
        }
        for expected in model.keys() {
            // SAFETY: this fixture performs only insertion, never reclamation.
            let raw = unsafe { array.lookup(*expected) }.cast::<usize>();
            // SAFETY: inserted Box remains array-owned; no removal or mutable
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
                    match unsafe { array.insert(usize::MAX, offered) } {
                        Ok(()) => true,
                        Err(returned) => {
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
    let array = XArray::<Box<u8>>::new();
    // SAFETY: primitive payload and host allocator, no kernel context needed.
    assert!(unsafe { array.insert(0, Box::new(7)) }.is_ok());
    // SAFETY: only insertions follow; array retains this path and owner.
    let raw = unsafe { array.lookup(0) }.cast::<u8>();
    // SAFETY: insertion-only storage retains the Box through array destruction;
    // later growth modifies atomic links, never the borrowed payload.
    let borrowed = unsafe { &*raw };
    for bit in (6..usize::BITS).step_by(6) {
        // SAFETY: same host context and primitive ownership as above.
        assert!(unsafe { array.insert(1 << bit, Box::new(9)) }.is_ok());
        assert_eq!(*borrowed, 7);
        // SAFETY: only insertion occurs, retaining all previously published nodes.
        assert_eq!(unsafe { array.lookup(0) }.cast::<u8>(), raw);
    }
    let units = XArray::<Box<()>>::new();
    for key in [0, 64, usize::MAX] {
        // SAFETY: unit has no destructor and ZST Box requires no deallocation.
        assert!(unsafe { units.insert(key, Box::new(())) }.is_ok());
        // SAFETY: insertion-only fixture retains the path and aligned ZST owner.
        let raw = unsafe { units.lookup(key) }.cast::<()>();
        // SAFETY: the array retains this aligned ZST owner throughout the borrow.
        assert_eq!(unsafe { raw.as_ref() }, Some(&()));
    }
}

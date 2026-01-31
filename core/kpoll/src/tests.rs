//! Unit tests for kpoll using the unittest framework.

#![cfg(unittest)]

use alloc::boxed::Box;
use core::{
    sync::atomic::{AtomicUsize, Ordering},
    task::{RawWaker, RawWakerVTable, Waker},
};

use unittest::{assert, assert_eq, def_test};

use super::{POLL_SET_CAPACITY, PollSet, PollSetGroup};

fn new_counter() -> &'static AtomicUsize {
    Box::leak(Box::new(AtomicUsize::new(0)))
}

unsafe fn waker_clone(data: *const ()) -> RawWaker {
    RawWaker::new(data, &WAKER_VTABLE)
}

unsafe fn waker_wake(data: *const ()) {
    let counter = unsafe { &*(data as *const AtomicUsize) };
    counter.fetch_add(1, Ordering::SeqCst);
}

unsafe fn waker_wake_by_ref(data: *const ()) {
    let counter = unsafe { &*(data as *const AtomicUsize) };
    counter.fetch_add(1, Ordering::SeqCst);
}

unsafe fn waker_drop(_data: *const ()) {}

static WAKER_VTABLE: RawWakerVTable =
    RawWakerVTable::new(waker_clone, waker_wake, waker_wake_by_ref, waker_drop);

fn make_waker(counter: &'static AtomicUsize) -> Waker {
    let raw = RawWaker::new(counter as *const _ as *const (), &WAKER_VTABLE);
    unsafe { Waker::from_raw(raw) }
}

#[def_test]
fn test_pollset_register_and_wake() {
    let set = PollSet::new();
    let counter = new_counter();
    let waker = make_waker(counter);

    set.register(&waker);
    set.register(&waker);

    let woke = set.wake();
    unittest::assert_eq!(woke, 2);
    unittest::assert_eq!(counter.load(Ordering::SeqCst), 2);
}

#[def_test]
fn test_pollset_capacity_eviction_wakes_old() {
    let set = PollSet::new();
    let counter_old = new_counter();
    let waker_old = make_waker(counter_old);

    for _ in 0..POLL_SET_CAPACITY {
        set.register(&waker_old);
    }

    let counter_new = new_counter();
    let waker_new = make_waker(counter_new);
    set.register(&waker_new);

    assert_eq!(counter_old.load(Ordering::SeqCst), 1);
}

#[def_test]
fn test_pollset_group_wake_all() {
    let mut group = PollSetGroup::new();
    let set_a = PollSet::new();
    let set_b = PollSet::new();

    group.add(set_a);
    group.add(set_b);

    let counter = new_counter();
    let waker = make_waker(counter);

    group.register_all(&waker);

    let woke = group.wake_all();
    assert_eq!(woke, 2);
    assert!(counter.load(Ordering::SeqCst) >= 2);
}

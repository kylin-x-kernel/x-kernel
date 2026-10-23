// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, vec::Vec};
use core::{
    sync::atomic::{AtomicUsize, Ordering},
    task::{Context, RawWaker, RawWakerVTable, Waker},
};

use unittest::{assert, assert_eq, def_test};

use super::{PollEvent, PollRegistrations, PollSet, WeakPollSet};

#[def_test]
fn test_weak_source_cancel_wake_and_expiry() {
    let source = PollSet::new();
    let weak = source.downgrade();
    let counter = new_counter();
    let waker = make_waker(counter);
    let context = Context::from_waker(&waker);
    let mut registrations = PollRegistrations::new();
    registrations
        .context(&context)
        .register_weak(&weak)
        .unwrap();
    assert_eq!(registrations.len(), 1);
    registrations.clear();
    assert_eq!(source.wake(), 0);
    registrations
        .context(&context)
        .register_weak(&weak)
        .unwrap();
    assert_eq!(source.wake(), 1);
    registrations.clear();
    registrations
        .context(&context)
        .register_weak(&weak)
        .unwrap();
    drop(source);
    assert_eq!(counter.load(Ordering::SeqCst), 2);
    registrations
        .context(&context)
        .register_weak(&weak)
        .unwrap();
    assert!(registrations.is_empty());
    assert_eq!(counter.load(Ordering::SeqCst), 2);
}

struct LastStrongRelease {
    owner: kspin::SpinNoIrq<Option<PollSet>>,
    source: WeakPollSet,
    clones: AtomicUsize,
    wakes: AtomicUsize,
}

/// # Safety
///
/// `data` must point to the boxed `LastStrongRelease` installed with this
/// vtable. The box must stay live for every clone; callbacks only borrow the
/// state and never deallocate it.
unsafe fn last_strong_clone(data: *const ()) -> RawWaker {
    // SAFETY: the test keeps the boxed state alive until all wakers are dropped.
    let state = unsafe { &*data.cast::<LastStrongRelease>() };
    state.clones.fetch_add(1, Ordering::SeqCst);
    let owner = state.owner.lock().take();
    drop(owner);
    RawWaker::new(data, &LAST_STRONG_VTABLE)
}

/// # Safety
///
/// `data` must point to the boxed `LastStrongRelease` installed with this
/// vtable. The box must stay live for every clone; callbacks only borrow the
/// state and never deallocate it.
unsafe fn last_strong_wake(data: *const ()) {
    // SAFETY: the test keeps the boxed state alive until all wakers are dropped.
    let state = unsafe { &*data.cast::<LastStrongRelease>() };
    core::assert!(state.owner.lock().is_none());
    let context = Context::from_waker(Waker::noop());
    let mut registrations = PollRegistrations::new();
    registrations
        .context(&context)
        .register_weak(&state.source)
        .unwrap();
    core::assert!(registrations.is_empty());
    state.wakes.fetch_add(1, Ordering::SeqCst);
}

static LAST_STRONG_VTABLE: RawWakerVTable = RawWakerVTable::new(
    last_strong_clone,
    last_strong_wake,
    last_strong_wake,
    waker_drop,
);

#[def_test]
fn test_weak_registration_last_temporary_owner_release_reenters() {
    let source = PollSet::new();
    let state = Box::new(LastStrongRelease {
        source: source.downgrade(),
        owner: kspin::SpinNoIrq::new(Some(source)),
        clones: AtomicUsize::new(0),
        wakes: AtomicUsize::new(0),
    });
    let raw = RawWaker::new(&*state as *const _ as *const (), &LAST_STRONG_VTABLE);
    // SAFETY: state is stable in its Box and outlives the waker, its clones and
    // registrations. Callbacks only borrow it; the no-op drop owns no reference.
    let waker = unsafe { Waker::from_raw(raw) };
    let context = Context::from_waker(&waker);
    let mut registrations = PollRegistrations::new();
    // Waker cloning removes the external owner after register_weak upgrades.
    registrations
        .context(&context)
        .register_weak(&state.source)
        .unwrap();
    assert_eq!(state.clones.load(Ordering::SeqCst), 1);
    assert_eq!(state.wakes.load(Ordering::SeqCst), 1);
    assert_eq!(registrations.len(), 1);
    registrations
        .context(&context)
        .register_weak(&state.source)
        .unwrap();
    assert!(registrations.is_empty());
    assert_eq!(state.clones.load(Ordering::SeqCst), 1);
}

fn new_counter() -> &'static AtomicUsize {
    Box::leak(Box::new(AtomicUsize::new(0)))
}

/// # Safety
///
/// `data` must be the aligned pointer to a live static `AtomicUsize`
/// installed by `make_waker`. It must remain valid for every cloned waker;
/// these callbacks borrow the counter and never reclaim its allocation.
unsafe fn waker_clone(data: *const ()) -> RawWaker {
    RawWaker::new(data, &WAKER_VTABLE)
}

/// # Safety
///
/// `data` must be the aligned pointer to a live static `AtomicUsize`
/// installed by `make_waker`. It must remain valid for every cloned waker;
/// these callbacks borrow the counter and never reclaim its allocation.
unsafe fn waker_wake(data: *const ()) {
    // SAFETY: `make_waker` installs a leaked, aligned `AtomicUsize` pointer.
    let counter = unsafe { &*(data as *const AtomicUsize) };
    counter.fetch_add(1, Ordering::SeqCst);
}

/// # Safety
///
/// `data` must be the aligned pointer to a live static `AtomicUsize`
/// installed by `make_waker`. It must remain valid for every cloned waker;
/// these callbacks borrow the counter and never reclaim its allocation.
unsafe fn waker_wake_by_ref(data: *const ()) {
    // SAFETY: `make_waker` installs a leaked, aligned `AtomicUsize` pointer.
    let counter = unsafe { &*(data as *const AtomicUsize) };
    counter.fetch_add(1, Ordering::SeqCst);
}

/// # Safety
///
/// `data` must be the aligned pointer to a live static `AtomicUsize`
/// installed by `make_waker`. It must remain valid for every cloned waker;
/// these callbacks borrow the counter and never reclaim its allocation.
unsafe fn waker_drop(_data: *const ()) {}

static WAKER_VTABLE: RawWakerVTable =
    RawWakerVTable::new(waker_clone, waker_wake, waker_wake_by_ref, waker_drop);

fn make_waker(counter: &'static AtomicUsize) -> Waker {
    let raw = RawWaker::new(counter as *const _ as *const (), &WAKER_VTABLE);
    // SAFETY: all vtable operations preserve the leaked AtomicUsize pointer.
    unsafe { Waker::from_raw(raw) }
}

#[def_test]
fn test_registration_drop_unregisters_waiter() {
    let set = PollSet::new();
    let counter = new_counter();
    let waker = make_waker(counter);

    let registration = set.register(&waker).unwrap();
    assert!(registration.cancel());
    assert_eq!(set.wake(), 0);
    assert_eq!(counter.load(Ordering::SeqCst), 0);
}

#[def_test]
fn test_same_waker_has_independent_registrations() {
    let set = PollSet::new();
    let counter = new_counter();
    let waker = make_waker(counter);

    let first = set.register(&waker).unwrap();
    let _second = set.register(&waker).unwrap();
    assert!(first.cancel());
    assert_eq!(set.wake(), 1);
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

/// More than the former 64-slot limit must remain registered.
#[def_test]
fn test_many_waiters_all_woken() {
    const N: usize = 80;
    let set = PollSet::new();
    let mut registrations = Vec::new();
    let mut counters = Vec::new();

    for _ in 0..N {
        let counter = new_counter();
        let waker = make_waker(counter);
        registrations.push(set.register(&waker).unwrap());
        counters.push(counter);
    }

    assert_eq!(set.wake(), N);
    for counter in &counters {
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }
}

#[def_test]
fn test_old_guard_cannot_cancel_reused_slot() {
    let set = PollSet::new();
    let first_counter = new_counter();
    let first = set.register(&make_waker(first_counter)).unwrap();

    assert_eq!(set.wake(), 1);
    let second_counter = new_counter();
    let _second = set.register(&make_waker(second_counter)).unwrap();
    drop(first);

    assert_eq!(set.wake(), 1);
    assert_eq!(first_counter.load(Ordering::SeqCst), 1);
    assert_eq!(second_counter.load(Ordering::SeqCst), 1);
}

struct ReentrantWake {
    set: &'static PollSet,
    count: AtomicUsize,
}

/// # Safety
///
/// `data` must point to the initialized, leaked `ReentrantWake` used
/// with this vtable. The state and its referenced polling object must remain
/// live for every clone; callbacks share the state and never deallocate it.
unsafe fn reentrant_clone(data: *const ()) -> RawWaker {
    RawWaker::new(data, &REENTRANT_VTABLE)
}

/// # Safety
///
/// `data` must point to the initialized, leaked `ReentrantWake` used
/// with this vtable. The state and its referenced polling object must remain
/// live for every clone; callbacks share the state and never deallocate it.
unsafe fn reentrant_wake(data: *const ()) {
    // SAFETY: the pointer is created from a leaked `ReentrantWake`.
    let state = unsafe { &*(data as *const ReentrantWake) };
    state.count.fetch_add(1, Ordering::SeqCst);
    // Use core::assert: unittest macros return TestResult and cannot appear
    // inside RawWaker callbacks.
    core::assert!(state.set.wake() == 0);
}

/// # Safety
///
/// `data` must point to the initialized, leaked `ReentrantWake` used
/// with this vtable. The state and its referenced polling object must remain
/// live for every clone; callbacks share the state and never deallocate it.
unsafe fn reentrant_wake_by_ref(data: *const ()) {
    // SAFETY: same invariant as `reentrant_wake`.
    unsafe { reentrant_wake(data) };
}

/// # Safety
///
/// `data` must point to the initialized, leaked `ReentrantWake` used
/// with this vtable. The state and its referenced polling object must remain
/// live for every clone; callbacks share the state and never deallocate it.
unsafe fn reentrant_drop(_data: *const ()) {}

static REENTRANT_VTABLE: RawWakerVTable = RawWakerVTable::new(
    reentrant_clone,
    reentrant_wake,
    reentrant_wake_by_ref,
    reentrant_drop,
);

#[def_test]
fn test_wake_is_reentrant_after_unlock() {
    let set = Box::leak(Box::new(PollSet::new()));
    let state = Box::leak(Box::new(ReentrantWake {
        set,
        count: AtomicUsize::new(0),
    }));
    let raw = RawWaker::new(state as *const _ as *const (), &REENTRANT_VTABLE);
    // SAFETY: the vtable operates on the leaked `ReentrantWake`.
    let waker = unsafe { Waker::from_raw(raw) };
    let _registration = set.register(&waker).unwrap();

    assert_eq!(set.wake(), 1);
    assert_eq!(state.count.load(Ordering::SeqCst), 1);
}

#[def_test]
fn test_source_drop_wakes_waiter() {
    let counter = new_counter();
    let registration = {
        let set = PollSet::new();
        let registration = set.register(&make_waker(counter)).unwrap();
        drop(set);
        registration
    };
    assert_eq!(counter.load(Ordering::SeqCst), 1);
    assert!(!registration.cancel());
}

#[def_test]
fn test_poll_event_broadcasts_generation_change_to_all_waiters() {
    let event = PollEvent::new();
    let observed_generation = event.generation();
    let first_counter = new_counter();
    let second_counter = new_counter();
    let first_waker = make_waker(first_counter);
    let second_waker = make_waker(second_counter);
    let mut first_registrations = PollRegistrations::new();
    let mut second_registrations = PollRegistrations::new();

    event
        .register(&mut first_registrations.context(&Context::from_waker(&first_waker)))
        .expect("event waiter registration should succeed");
    event
        .register(&mut second_registrations.context(&Context::from_waker(&second_waker)))
        .expect("event waiter registration should succeed");

    let wake_set = event.notify_defer_wake();
    assert!(event.has_changed_since(observed_generation));
    assert_eq!(first_counter.load(Ordering::SeqCst), 0);
    assert_eq!(second_counter.load(Ordering::SeqCst), 0);

    assert_eq!(wake_set.wake(), 2);
    assert_eq!(first_counter.load(Ordering::SeqCst), 1);
    assert_eq!(second_counter.load(Ordering::SeqCst), 1);

    drop(first_registrations);
    drop(second_registrations);
}

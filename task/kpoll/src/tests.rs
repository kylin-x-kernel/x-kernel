// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unit tests for kpoll using the unittest framework.

#![cfg(unittest)]

use alloc::{boxed::Box, vec::Vec};
use core::{
    sync::atomic::{AtomicUsize, Ordering},
    task::{Context, RawWaker, RawWakerVTable, Waker},
};

use unittest::{assert, assert_eq, def_test};

use super::{Completion, PollEvent, PollRegistrations, PollSet};

fn new_counter() -> &'static AtomicUsize {
    Box::leak(Box::new(AtomicUsize::new(0)))
}

unsafe fn waker_clone(data: *const ()) -> RawWaker {
    RawWaker::new(data, &WAKER_VTABLE)
}

unsafe fn waker_wake(data: *const ()) {
    // SAFETY: `make_waker` installs a leaked, aligned `AtomicUsize` pointer.
    let counter = unsafe { &*(data as *const AtomicUsize) };
    counter.fetch_add(1, Ordering::SeqCst);
}

unsafe fn waker_wake_by_ref(data: *const ()) {
    // SAFETY: `make_waker` installs a leaked, aligned `AtomicUsize` pointer.
    let counter = unsafe { &*(data as *const AtomicUsize) };
    counter.fetch_add(1, Ordering::SeqCst);
}

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

unsafe fn reentrant_clone(data: *const ()) -> RawWaker {
    RawWaker::new(data, &REENTRANT_VTABLE)
}

unsafe fn reentrant_wake(data: *const ()) {
    // SAFETY: the pointer is created from a leaked `ReentrantWake`.
    let state = unsafe { &*(data as *const ReentrantWake) };
    state.count.fetch_add(1, Ordering::SeqCst);
    // Use core::assert: unittest macros return TestResult and cannot appear
    // inside RawWaker callbacks.
    core::assert!(state.set.wake() == 0);
}

unsafe fn reentrant_wake_by_ref(data: *const ()) {
    // SAFETY: same invariant as `reentrant_wake`.
    unsafe { reentrant_wake(data) };
}

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
fn test_completion_initial_state_is_not_ready() {
    let completion = Completion::new();

    assert!(!completion.is_completed());
    assert!(!completion.try_wait());
}

#[def_test]
fn test_completion_complete_releases_one_token() {
    let completion = Completion::new();

    assert_eq!(completion.complete(), 0);
    assert!(completion.is_completed());
    assert!(completion.try_wait());
    assert!(!completion.is_completed());
    assert!(!completion.try_wait());
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

#[def_test]
fn test_completion_accumulates_tokens() {
    let completion = Completion::new();

    assert_eq!(completion.complete(), 0);
    assert_eq!(completion.complete(), 0);

    assert!(completion.try_wait());
    assert!(completion.try_wait());
    assert!(!completion.try_wait());
}

#[def_test]
fn test_completion_complete_all_is_permanent_until_reinit() {
    let completion = Completion::new();

    assert_eq!(completion.complete_all(), 0);
    assert!(completion.try_wait());
    assert!(completion.try_wait());
    assert!(completion.is_completed());

    completion.reinit();
    assert!(!completion.is_completed());
    assert!(!completion.try_wait());
}

#[def_test]
fn test_completion_complete_all_defer_wake_delays_waiter_wake() {
    let completion = Completion::new();
    let counter = new_counter();
    let waker = make_waker(counter);
    let registrations = register_completion_waiter(&completion, &waker);

    let wake_set = completion.complete_all_defer_wake();
    assert!(completion.is_completed());
    assert_eq!(counter.load(Ordering::SeqCst), 0);

    assert_eq!(wake_set.wake(), 1);
    assert_eq!(counter.load(Ordering::SeqCst), 1);
    drop(registrations);
    assert!(completion.try_wait());
}

fn register_completion_waiter(completion: &Completion, waker: &Waker) -> PollRegistrations {
    let mut registrations = PollRegistrations::new();
    let context = Context::from_waker(waker);
    completion
        .register(&mut registrations.context(&context))
        .unwrap();
    registrations
}

#[def_test]
fn test_completion_register_then_complete_wakes_waiter() {
    let completion = Completion::new();
    let counter = new_counter();
    let waker = make_waker(counter);
    let registrations = register_completion_waiter(&completion, &waker);

    assert_eq!(completion.complete(), 1);
    assert_eq!(counter.load(Ordering::SeqCst), 1);
    drop(registrations);
    assert!(completion.try_wait());
}

struct ReentrantCompletionWake {
    completion: &'static Completion,
    count: AtomicUsize,
}

unsafe fn reentrant_completion_clone(data: *const ()) -> RawWaker {
    RawWaker::new(data, &REENTRANT_COMPLETION_VTABLE)
}

unsafe fn reentrant_completion_wake(data: *const ()) {
    // SAFETY: the pointer is created from a leaked `ReentrantCompletionWake`.
    let state = unsafe { &*(data as *const ReentrantCompletionWake) };
    state.count.fetch_add(1, Ordering::SeqCst);
    core::assert!(state.completion.try_wait());
}

unsafe fn reentrant_completion_wake_by_ref(data: *const ()) {
    // SAFETY: same invariant as `reentrant_completion_wake`.
    unsafe { reentrant_completion_wake(data) };
}

unsafe fn reentrant_completion_drop(_data: *const ()) {}

static REENTRANT_COMPLETION_VTABLE: RawWakerVTable = RawWakerVTable::new(
    reentrant_completion_clone,
    reentrant_completion_wake,
    reentrant_completion_wake_by_ref,
    reentrant_completion_drop,
);

#[def_test]
fn test_completion_wake_is_reentrant_after_unlock() {
    let completion = Box::leak(Box::new(Completion::new()));
    let state = Box::leak(Box::new(ReentrantCompletionWake {
        completion,
        count: AtomicUsize::new(0),
    }));
    let raw = RawWaker::new(state as *const _ as *const (), &REENTRANT_COMPLETION_VTABLE);
    // SAFETY: the vtable operates on the leaked `ReentrantCompletionWake`.
    let waker = unsafe { Waker::from_raw(raw) };
    let _registrations = register_completion_waiter(completion, &waker);

    assert_eq!(completion.complete(), 1);
    assert_eq!(state.count.load(Ordering::SeqCst), 1);
    assert!(!completion.try_wait());
}

#[def_test]
fn test_completion_recheck_observes_complete_before_register() {
    let completion = Completion::new();
    let counter = new_counter();
    let waker = make_waker(counter);

    assert!(!completion.try_wait());
    assert_eq!(completion.complete(), 0);

    let _registrations = register_completion_waiter(&completion, &waker);
    assert!(completion.try_wait());
    assert_eq!(counter.load(Ordering::SeqCst), 0);
}

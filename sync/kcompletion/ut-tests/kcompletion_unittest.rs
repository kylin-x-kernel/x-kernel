// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use unittest::{assert, assert_eq, def_test};

use super::{Completion, SyncWake, WaitQueue};

struct Counter(AtomicUsize);

// SAFETY: Notification only updates an atomic; it cannot allocate, block,
// unwind, retain borrowed storage, acquire other locks, or reenter a queue.
unsafe impl SyncWake for Counter {
    fn notify(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[def_test]
fn completion_tokens_sticky_and_reinit() {
    let completion = Completion::new();
    assert!(!completion.is_completed());
    assert_eq!(completion.complete(), 0);
    assert_eq!(completion.complete(), 0);
    assert!(completion.try_wait());
    assert!(completion.try_wait());
    assert!(!completion.try_wait());
    assert_eq!(completion.complete_all(), 0);
    assert!(completion.try_wait());
    assert!(completion.try_wait());
    completion.reinit();
    assert!(!completion.try_wait());
}

#[def_test]
fn saturation_matches_linux_sticky_completion() {
    let completion = Completion::new();
    completion.state.lock().done = usize::MAX - 1;
    assert_eq!(completion.complete(), 0);
    assert!(completion.try_wait());
    assert!(completion.try_wait());
    assert_eq!(completion.state.lock().done, usize::MAX);
}

#[def_test]
fn deferred_wake_after_reset_does_not_republish() {
    let completion = Arc::new(Completion::new());
    let deferred = completion.complete_all_defer_wake();
    completion.reinit();
    let wake = Counter(AtomicUsize::new(0));
    let mut deferred = Some(deferred);
    let mut parks = 0;
    let mut delayed_count = 0;
    let mut was_completed = true;
    {
        let wait_wake = &wake;
        let wait_park = || {
            parks += 1;
            if let Some(delayed) = deferred.take() {
                delayed_count = delayed.wake();
                was_completed = completion.is_completed();
            } else {
                completion.complete();
            }
        };
        // SAFETY: These test closures only access local state and notify; they do
        // not exit the task or bypass cleanup. Stack, source and wake remain live.
        unsafe { completion.wait_with(wait_wake, wait_park) }
    };
    assert_eq!(delayed_count, 1);
    assert!(!was_completed);
    assert_eq!(parks, 2);
    assert_eq!(wake.0.load(Ordering::Relaxed), 2);
}

#[def_test]
fn spurious_wakes_do_not_duplicate_registration() {
    let completion = Completion::new();
    let wake = Counter(AtomicUsize::new(0));
    let mut parks = 0;
    let mut notified = 0;
    {
        let wait_wake = &wake;
        let wait_park = || {
            parks += 1;
            if parks == 4 {
                notified = completion.complete_all();
            }
        };
        // SAFETY: These test closures only access local state and notify; they do
        // not exit the task or bypass cleanup. Stack, source and wake remain live.
        unsafe { completion.wait_with(wait_wake, wait_park) }
    };
    assert_eq!(notified, 1);
    assert_eq!(wake.0.load(Ordering::Relaxed), 1);
    assert_eq!(completion.complete_all(), 0);
}

#[def_test]
fn token_stealing_reenrolls_and_fifo_notifies_one() {
    let completion = Completion::new();
    let first = Counter(AtomicUsize::new(0));
    let second = Counter(AtomicUsize::new(0));
    let mut parks = 0;
    let mut first_notified = 0;
    let mut first_count = 0;
    let mut second_count = 0;
    let mut retry_notified = 0;
    {
        let wait_wake = &first;
        let wait_park = || {
            parks += 1;
            if parks == 1 {
                {
                    let wait_wake = &second;
                    let wait_park = || {
                        first_notified = completion.complete();
                        first_count = first.0.load(Ordering::Relaxed);
                        second_count = second.0.load(Ordering::Relaxed);
                    };
                    // SAFETY: These test closures only access local state and notify; they do
                    // not exit the task or bypass cleanup. Stack, source and wake remain live.
                    unsafe { completion.wait_with(wait_wake, wait_park) }
                };
            } else {
                retry_notified = completion.complete();
            }
        };
        // SAFETY: These test closures only access local state and notify; they do
        // not exit the task or bypass cleanup. Stack, source and wake remain live.
        unsafe { completion.wait_with(wait_wake, wait_park) }
    };
    assert_eq!(parks, 2);
    assert_eq!(first_notified, 1);
    assert_eq!(first_count, 1);
    assert_eq!(second_count, 0);
    assert_eq!(retry_notified, 1);
    assert_eq!(first.0.load(Ordering::Relaxed), 2);
    assert_eq!(completion.complete_all(), 0);
}

#[def_test]
fn deferred_wake_retains_owner_and_cleans_stale_registration() {
    let completion = Arc::new(Completion::new());
    let wake = Counter(AtomicUsize::new(0));
    let mut deferred = None;
    {
        let wait_wake = &wake;
        let wait_park = || {
            deferred = Some(completion.complete_all_defer_wake());
        };
        // SAFETY: These test closures only access local state and notify; they do
        // not exit the task or bypass cleanup. Stack, source and wake remain live.
        unsafe { completion.wait_with(wait_wake, wait_park) }
    };
    let weak = Arc::downgrade(&completion);
    drop(completion);
    assert!(weak.upgrade().is_some());
    assert_eq!(deferred.take().unwrap().wake(), 0);
    assert!(weak.upgrade().is_none());
    assert_eq!(wake.0.load(Ordering::Relaxed), 0);
}

#[def_test]
fn predicate_reentry_and_spurious_wakes_reenroll() {
    let queue = WaitQueue::new();
    let wake = Counter(AtomicUsize::new(0));
    let mut checks = 0;
    let mut parks = 0;
    let mut notified = [0; 4];
    {
        let wait_wake = &wake;
        let wait_ready = || {
            checks += 1;
            notified[checks - 1] = queue.notify_all();
            checks == 4
        };
        let wait_park = || parks += 1;
        // SAFETY: These test closures only access local state and notify; they do
        // not exit the task or bypass cleanup. Stack, source and wake remain live.
        unsafe { queue.wait_until(wait_wake, wait_ready, wait_park) }
    };
    assert_eq!(parks, 3);
    assert_eq!(notified, [1; 4]);
    assert_eq!(wake.0.load(Ordering::Relaxed), 4);
    assert_eq!(queue.notify_all(), 0);
}

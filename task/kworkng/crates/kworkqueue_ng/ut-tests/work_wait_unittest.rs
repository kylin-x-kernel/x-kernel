// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, sync::Arc};
use core::{
    assert, assert_eq,
    sync::atomic::{AtomicBool, Ordering},
};

use unittest::def_test;

use super::*;
use crate::work::Work;

/// Abandoned preparation releases every unattached receipt.
#[def_test]
fn abandoned_preparation_releases_receipts() {
    let wait = CleanupWait::prepare(2);
    let receipts: Vec<_> = wait.entries.iter().map(Arc::downgrade).collect();
    drop(wait);
    assert!(receipts.iter().all(|receipt| receipt.upgrade().is_none()));
}

/// A later receipt may finish before the sequential waiter reaches it.
#[def_test]
fn sequential_wait_preserves_early_completion() {
    let mut wait = CleanupWait::prepare(2);
    let mut later = crate::work::observers::CompletionObservers::new();
    later.attach(wait.prepared.pop().unwrap());
    let mut first = crate::work::observers::CompletionObservers::new();
    first.attach(wait.prepared.pop().unwrap());
    let waiter = ktask::spawn(move || wait.wait(&CompletionWait::prepare().unwrap()));
    while waiter.state() != ktask::TaskState::Blocked {
        ktask::yield_now();
    }
    later.complete();
    assert_eq!(waiter.state(), ktask::TaskState::Blocked);
    first.complete();
    assert_eq!(waiter.join(), 0);
}

/// A shrinking capture must wait only for the consumed suffix of prepared observers.
#[def_test]
fn unused_observers_do_not_extend_capture() {
    let work = Work::new();
    assert!(work.accept(()).is_ok());
    let mut wait = CleanupWait::prepare(2);
    assert_eq!(work.capture(&mut wait.prepared), Ok(1));
    wait.discard_unused();
    assert!(!wait.is_completed());
    assert!(work.cancel_pending());
    assert!(wait.is_completed());
}

/// Idle and pending stops release only their own disable; sync disable returns ownership.
#[def_test]
fn synchronous_stop_preserves_independent_disable_ownership() {
    let work = Work::new();
    assert_eq!(work.cancel_sync(), Ok(false));
    assert!(work.accept(()).is_ok());
    let (guard, revoked) = work.disable_sync().unwrap();
    assert!(revoked);
    assert!(work.accept(()).is_err());
    assert_eq!(work.cancel_sync(), Ok(false));
    assert!(work.accept(()).is_err());
    drop(guard);
    assert!(work.accept(()).is_ok());
    assert_eq!(work.cancel_sync(), Ok(true));
    assert!(!work.state.lock().is_disabled());
}

/// Canceling a ready Work synchronously returns its partition allowance to a waiter.
#[def_test]
fn synchronous_disable_refills_partition() {
    let partition = Arc::new(crate::queue::partition::QueuePartition::new(
        kcpu_id_map::LogicalCpuId::new(0),
        core::num::NonZeroUsize::new(1).unwrap(),
    ));
    let first = core::pin::pin!(Work::new());
    let second = core::pin::pin!(Work::new());
    // SAFETY: both pinned Works remain live until cancellation/execution drain below.
    assert!(unsafe { partition.accept(&first, (), 0) }.is_ok());
    // SAFETY: second remains pinned and is executed before leaving this scope.
    assert!(unsafe { partition.accept(&second, (), 0) }.is_ok());
    let (guard, revoked) = first.disable_sync().unwrap();
    assert!(revoked);
    assert_eq!(first.observer_count(), 0);
    partition.claim_next().unwrap().run();
    assert_eq!(second.observer_count(), 0);
    assert!(partition.claim_next().is_none());
    drop(guard);
}

/// Invalid context rejection occurs before pending cancellation or disable.
#[def_test]
fn synchronous_stop_rejects_before_effects() {
    let work = Work::new();
    assert!(work.accept(()).is_ok());
    {
        let _masked = kspin::NoPreemptIrqSave::new();
        assert_eq!(work.cancel_sync(), Err(WaitError::InvalidContext));
    }
    assert_eq!(work.observer_count(), 1);
    assert!(!work.state.lock().is_disabled());
    assert!(work.cancel_pending());
    {
        let _masked = kspin::NoPreemptIrqSave::new();
        assert_eq!(work.cancel_sync(), Err(WaitError::InvalidContext));
    }
}

/// Real callbacks reject self-stop before touching a pending successor or disable.
///
/// # Panics
/// Panics on missing identity, changed pending/disable state, or stale identity on reuse.
#[cfg(feature = "self-wait-check")]
#[def_test]
fn callback_self_stop_preserves_successor_and_allows_reuse() {
    type CallbackWork = Work<(), Box<dyn Fn() + Send + Sync>>;
    let work: Arc<CallbackWork> = Arc::new_cyclic(|weak: &alloc::sync::Weak<CallbackWork>| {
        let weak = weak.clone();
        Work::with_callback(Box::new(move || {
            let work = weak.upgrade().unwrap();
            assert!(work.accept(()).is_ok());
            assert_eq!(work.cancel_sync(), Err(WaitError::SelfWait));
            assert_eq!(work.flush(), Err(WaitError::SelfWait));
            assert!(matches!(work.disable_sync(), Err(WaitError::SelfWait)));
            ktask::yield_now();
            assert_eq!(work.cancel_sync(), Err(WaitError::SelfWait));
            assert_eq!(work.observer_count(), 2);
            assert!(!work.state.lock().is_disabled());
        }))
    });
    assert!(work.accept(()).is_ok());
    work.claim().unwrap().run();
    // The same task is no longer the callback, so stopping its successor is legal.
    assert_eq!(work.cancel_sync(), Ok(true));
    assert!(work.accept(()).is_ok());
    work.claim().unwrap().run();
    assert_eq!(work.cancel_sync(), Ok(true));
}

/// Nested callbacks retain the outer identity without rejecting unrelated Work.
///
/// # Panics
/// Panics on a lost outer identity or false-positive idle wait.
#[cfg(feature = "self-wait-check")]
#[def_test]
fn nested_callbacks_preserve_outer_self_wait_identity() {
    type CallbackWork = Work<(), Box<dyn Fn() + Send + Sync>>;
    let outer: Arc<CallbackWork> = Arc::new_cyclic(|weak: &alloc::sync::Weak<CallbackWork>| {
        let weak = weak.clone();
        Work::with_callback(Box::new(move || {
            let outer = weak.upgrade().unwrap();
            let inner = Work::with_callback(|| {
                assert_eq!(outer.cancel_sync(), Err(WaitError::SelfWait));
                let unrelated = Work::<()>::new();
                assert_eq!(unrelated.cancel_sync(), Ok(false));
            });
            assert!(inner.accept(()).is_ok());
            inner.claim().unwrap().run();
            assert_eq!(inner.cancel_sync(), Ok(false));
            assert_eq!(outer.cancel_sync(), Err(WaitError::SelfWait));
        }))
    });
    assert!(outer.accept(()).is_ok());
    outer.claim().unwrap().run();
    assert_eq!(outer.cancel_sync(), Ok(false));
}

struct Resource(Option<Box<dyn FnOnce() + Send>>);

impl Drop for Resource {
    fn drop(&mut self) {
        if let Some(cleanup) = self.0.take() {
            cleanup();
        }
    }
}

fn until(flag: &AtomicBool) {
    while !flag.load(Ordering::Acquire) {
        ktask::yield_now();
    }
}

/// The actual waiter parks through both callback and resource cleanup, then reopens admission.
#[def_test]
fn synchronous_cancel_waits_through_running_cleanup() {
    let callback_started = Arc::new(AtomicBool::new(false));
    let release_callback = Arc::new(AtomicBool::new(false));
    let cleanup_started = Arc::new(AtomicBool::new(false));
    let release_cleanup = Arc::new(AtomicBool::new(false));
    let returned = Arc::new(AtomicBool::new(false));
    let work = Arc::new(Work::with_callback({
        let (started, release) = (callback_started.clone(), release_callback.clone());
        move || {
            started.store(true, Ordering::Release);
            until(&release);
        }
    }));
    let resource = Resource(Some(Box::new({
        let (started, release) = (cleanup_started.clone(), release_cleanup.clone());
        move || {
            started.store(true, Ordering::Release);
            until(&release);
        }
    })));
    assert!(work.accept(resource).is_ok());
    let runner = ktask::spawn({
        let work = work.clone();
        move || work.claim().unwrap().run()
    });
    until(&callback_started);
    // A successor is canceled by the stop, while its predecessor must finish normally.
    assert!(work.accept(Resource(None)).is_ok());
    let stopper = ktask::spawn({
        let (work, returned) = (work.clone(), returned.clone());
        move || {
            assert_eq!(work.cancel_sync(), Ok(true));
            returned.store(true, Ordering::Release);
        }
    });
    while stopper.state() != ktask::TaskState::Blocked {
        ktask::yield_now();
    }
    assert!(work.state.lock().is_disabled());
    assert!(!returned.load(Ordering::Acquire));
    release_callback.store(true, Ordering::Release);
    until(&cleanup_started);
    assert!(!returned.load(Ordering::Acquire));
    assert!(work.state.lock().is_disabled());
    release_cleanup.store(true, Ordering::Release);
    assert_eq!(runner.join(), 0);
    assert_eq!(stopper.join(), 0);
    assert!(returned.load(Ordering::Acquire));
    assert!(!work.state.lock().is_disabled());
    assert!(work.accept(Resource(None)).is_ok());
    assert!(work.cancel_pending());
}

/// Concurrent synchronous callers observe the same execution without releasing another owner.
#[def_test]
fn concurrent_synchronous_cancellations_keep_external_disable() {
    let started = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let work = Arc::new(Work::with_callback({
        let (started, release) = (started.clone(), release.clone());
        move || {
            started.store(true, Ordering::Release);
            until(&release);
        }
    }));
    assert!(work.accept(()).is_ok());
    let runner = ktask::spawn({
        let work = work.clone();
        move || work.claim().unwrap().run()
    });
    until(&started);
    let (external, revoked) = work.disable();
    assert!(!revoked);
    let stoppers = core::array::from_fn::<_, 2, _>(|_| {
        let work = work.clone();
        ktask::spawn(move || assert_eq!(work.cancel_sync(), Ok(false)))
    });
    while stoppers
        .iter()
        .any(|task| task.state() != ktask::TaskState::Blocked)
    {
        ktask::yield_now();
    }
    release.store(true, Ordering::Release);
    assert_eq!(runner.join(), 0);
    for stopper in stoppers {
        assert_eq!(stopper.join(), 0);
    }
    assert!(work.state.lock().is_disabled());
    drop(external);
    assert!(!work.state.lock().is_disabled());
}

/// Flush captures pending/running once and never disables or follows later work.
///
/// # Panics
/// Panics on early completion, unintended cancellation/disable or successor capture.
#[def_test]
fn flush_waits_for_captured_pair_but_not_later_successor() {
    let work = Arc::new(Work::new());
    assert!(work.accept(()).is_ok());
    let running = work.claim().unwrap();
    assert!(work.accept(()).is_ok());
    let flushers = core::array::from_fn::<_, 2, _>(|_| {
        let work = work.clone();
        ktask::spawn(move || assert_eq!(work.flush(), Ok(())))
    });
    while flushers
        .iter()
        .any(|task| task.state() != ktask::TaskState::Blocked)
    {
        ktask::yield_now();
    }
    assert!(!work.state.lock().is_disabled());
    assert_eq!(work.observer_count(), 2);
    running.run();
    let successor = work.claim().unwrap();
    assert!(work.accept(()).is_ok());
    successor.run();
    for flusher in flushers {
        assert_eq!(flusher.join(), 0);
    }
    assert_eq!(work.observer_count(), 1);
    assert!(work.cancel_pending());
    assert_eq!(work.flush(), Ok(()));
}

/// Captured cancellation cleanup remains owed, but a later capture excludes it.
///
/// # Panics
/// Panics on early return, retained old cleanup in a fresh wait or disable effects.
#[def_test]
fn flush_tracks_canceled_cleanup_only_if_already_captured() {
    let work = Arc::new(Work::new());
    let started = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let returned = Arc::new(AtomicBool::new(false));
    assert!(
        work.accept(Resource(Some(Box::new({
            let (started, release) = (started.clone(), release.clone());
            move || {
                started.store(true, Ordering::Release);
                until(&release);
            }
        }))))
        .is_ok()
    );
    let flusher = ktask::spawn({
        let (work, returned) = (work.clone(), returned.clone());
        move || {
            assert_eq!(work.flush(), Ok(()));
            returned.store(true, Ordering::Release);
        }
    });
    while flusher.state() != ktask::TaskState::Blocked {
        ktask::yield_now();
    }
    let cancel = ktask::spawn({
        let work = work.clone();
        move || assert!(work.cancel_pending())
    });
    until(&started);
    assert!(!returned.load(Ordering::Acquire));
    assert_eq!(work.flush(), Ok(()));
    assert!(!work.state.lock().is_disabled());
    assert!(work.accept(Resource(None)).is_ok());
    release.store(true, Ordering::Release);
    assert_eq!(cancel.join(), 0);
    assert_eq!(flusher.join(), 0);
    assert!(work.cancel_pending());
}

/// Context validation applies even to idle Work, without affecting pending work.
///
/// # Panics
/// Panics on accepted invalid context, pending loss or leaked disable ownership.
#[def_test]
fn flush_rejects_invalid_context_even_when_idle() {
    let work = Work::new();
    {
        let _guard = kspin::NoPreemptIrqSave::new();
        assert_eq!(work.flush(), Err(WaitError::InvalidContext));
    }
    assert!(work.accept(()).is_ok());
    {
        let _guard = kspin::NoPreemptIrqSave::new();
        assert_eq!(work.flush(), Err(WaitError::InvalidContext));
    }
    assert_eq!(work.observer_count(), 1);
    assert!(!work.state.lock().is_disabled());
    assert!(work.cancel_pending());
}

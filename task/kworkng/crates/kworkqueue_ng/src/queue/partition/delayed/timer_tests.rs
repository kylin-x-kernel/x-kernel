// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Real prepared-timer IRQ integration with controlled ordinary execution.
//! The boot-initialized timer service stays active for the entire test run;
//! each case owns and synchronously drains its own timer without replacing hooks.

use alloc::sync::Arc;
use core::{
    pin::pin,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use ktime_types::{MonotonicInstant, TimeSpan};
use unittest::def_test;

use super::{ArmError, SubmitError};
use crate::{
    queue::tests::queue_for,
    work::{delayed::DelayedWork, wait::WaitError},
};

fn horizon() -> MonotonicInstant {
    MonotonicInstant::from_span_since_origin(TimeSpan::from_nanos(u64::MAX))
}

fn wait_ready<H: Fn() + Send + Sync>(work: &DelayedWork<H>) {
    while !work.work_for_test().state.lock().pending_is_ready() {
        ktask::yield_now();
    }
}

/// Real timer IRQ queues a borrowed-capture Work but never executes it inline.
///
/// # Panics
/// Panics on early business execution, missing delivery, duplicate work or leaked Queue.
#[def_test]
fn prepared_timer_irq_transfers_borrowed_work() {
    let queue = queue_for(&[0], 1);
    let calls = AtomicUsize::new(0);
    let work = pin!(DelayedWork::new(|| {
        calls.fetch_add(1, Ordering::Relaxed);
    }));
    {
        let _irq = kspin::NoPreemptIrqSave::new();
        // SAFETY: initialized test CPU, pinned Work and stack capture remain shared
        // through explicit ordinary execution and synchronized timer drain below.
        unsafe {
            work.as_ref()
                .arm_on(&queue, 0.into(), khal::time::monotonic_time())
        }
        .unwrap();
        assert!(
            work.work_for_test()
                .state
                .lock()
                .delayed_deadline_ns()
                .is_some()
        );
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }
    wait_ready(&work);
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    queue
        .partition(0.into())
        .unwrap()
        .claim_next()
        .unwrap()
        .run();
    assert!(!work.cancel_sync().unwrap());
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert!(queue.partition(0.into()).unwrap().claim_next().is_none());
    assert_eq!(Arc::strong_count(&queue), 1);
}

/// Repeated synchronous cancellation drains future and possibly already-delivering timers.
///
/// # Panics
/// Panics on missed cancellation, restored work, callback execution or leaked Queue.
#[def_test]
fn prepared_timer_cancel_and_reuse_preserve_storage() {
    let queue = queue_for(&[0], 1);
    let work = pin!(DelayedWork::new(|| panic!(
        "canceled business callback ran"
    )));
    for round in 0..32 {
        let deadline = if round % 2 == 0 {
            horizon()
        } else {
            khal::time::monotonic_time()
        };
        // SAFETY: the same pinned Work outlives every timer and ordinary cancellation;
        // no other producer uses it and current CPU timer service is initialized.
        unsafe { work.as_ref().arm_on(&queue, 0.into(), deadline) }.unwrap();
        assert!(work.cancel_sync().unwrap());
        assert!(!work.cancel_sync().unwrap());
        assert_eq!(work.work_for_test().observer_count(), 0);
        assert!(queue.partition(0.into()).unwrap().claim_next().is_none());
        assert_eq!(Arc::strong_count(&queue), 1);
    }
    queue.close_and_drain().unwrap();
}

/// Admission and context failures cannot replace or revoke a previously armed timer.
///
/// # Panics
/// Panics on changed placement after failure, a stuck timer-only flush or lost drain.
#[def_test]
fn prepared_timer_rejections_precede_effects() {
    let queue = queue_for(&[0], 1);
    let work = pin!(DelayedWork::new(|| {}));
    let invalid = MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(u64::MAX));
    assert_eq!(
        // SAFETY: idle pinned storage, initialized service, no timer can be accepted.
        unsafe { work.as_ref().arm_on(&queue, 0.into(), invalid) },
        Err(ArmError::Timer(kerrno::KError::InvalidInput))
    );
    // SAFETY: retained pinned storage is synchronously canceled before release.
    unsafe { work.as_ref().arm_on(&queue, 0.into(), horizon()) }.unwrap();
    assert_eq!(
        // SAFETY: duplicate must leave the existing retained setting unchanged.
        unsafe {
            work.as_ref()
                .arm_on(&queue, 0.into(), khal::time::monotonic_time())
        },
        Err(ArmError::Admission(SubmitError::AlreadyQueued))
    );
    assert_eq!(
        // SAFETY: conflicting target is rejected before any physical programming.
        unsafe { work.as_ref().arm_on(&queue, 1.into(), horizon()) },
        Err(ArmError::Admission(SubmitError::OwnerConflict))
    );
    {
        let _irq = kspin::NoPreemptIrqSave::new();
        assert_eq!(work.cancel_sync(), Err(WaitError::InvalidContext));
    }
    assert_eq!(
        work.work_for_test().state.lock().delayed_deadline_ns(),
        Some(u64::MAX)
    );
    queue.flush().unwrap();
    assert!(work.cancel_sync().unwrap());
    queue.close_and_drain().unwrap();
    assert_eq!(
        // SAFETY: closed Queue rejects while the same pinned storage remains live.
        unsafe { work.as_ref().arm_on(&queue, 0.into(), horizon()) },
        Err(ArmError::Admission(SubmitError::Closed))
    );
    assert!(!work.cancel_sync().unwrap());
}

/// The temporary disable spans a sleeping predecessor and timer-side successor drain.
///
/// # Panics
/// Panics on early cancel return, admission during drain, bad joins or lost reuse.
#[def_test]
fn prepared_timer_cancel_keeps_admission_closed_through_running_work() {
    let queue = queue_for(&[0], 1);
    let entered = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let work = Arc::pin(DelayedWork::new({
        let (entered, release) = (entered.clone(), release.clone());
        move || {
            entered.store(true, Ordering::Release);
            while !release.load(Ordering::Acquire) {
                ktask::yield_now();
            }
        }
    }));
    // SAFETY: outer pinned Arc and task clones retain Work through both joined drains.
    unsafe {
        work.as_ref()
            .arm_on(&queue, 0.into(), khal::time::monotonic_time())
    }
    .unwrap();
    wait_ready(&work);
    let runner = ktask::spawn({
        let queue = queue.clone();
        move || {
            queue
                .partition(0.into())
                .unwrap()
                .claim_next()
                .unwrap()
                .run()
        }
    });
    while !entered.load(Ordering::Acquire) {
        ktask::yield_now();
    }
    // SAFETY: same owner and stable Work; successor is revoked by the joined cancel.
    unsafe { work.as_ref().arm_on(&queue, 0.into(), horizon()) }.unwrap();
    let cancel = ktask::spawn({
        let work = work.clone();
        move || assert!(work.cancel_sync().unwrap())
    });
    while cancel.state() != ktask::TaskState::Blocked {
        ktask::yield_now();
    }
    assert_eq!(
        // SAFETY: retained Work is disabled while the cancel task waits for its predecessor.
        unsafe { work.as_ref().arm_on(&queue, 0.into(), horizon()) },
        Err(ArmError::Admission(SubmitError::Disabled))
    );
    release.store(true, Ordering::Release);
    assert_eq!(runner.join(), 0);
    assert_eq!(cancel.join(), 0);
    // SAFETY: successful drain permits reuse of the same still-pinned allocation.
    unsafe { work.as_ref().arm_on(&queue, 0.into(), horizon()) }.unwrap();
    assert!(work.cancel_sync().unwrap());
    assert_eq!(Arc::strong_count(&queue), 1);
}

/// Zero delay is ordinary queuing even while the local timer IRQ cannot run.
///
/// # Panics
/// Panics on timer placement, missing readiness, inline execution or leaked Queue.
#[def_test]
fn zero_delay_queues_without_timer_delivery() {
    let queue = queue_for(&[0], 1);
    let calls = AtomicUsize::new(0);
    let work = pin!(DelayedWork::new(|| {
        calls.fetch_add(1, Ordering::Relaxed);
    }));
    {
        let _irq = kspin::NoPreemptIrqSave::new();
        // SAFETY: pinned Work and borrowed capture stay shared through ordinary
        // execution and delayed drain; initialized test CPU supplies timer service.
        unsafe {
            work.as_ref()
                .queue_after_on(&queue, 0.into(), TimeSpan::ZERO)
        }
        .unwrap();
        let state = work.work_for_test().state.lock();
        assert!(state.pending_is_ready());
        assert_eq!(state.delayed_deadline_ns(), None);
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }
    queue
        .partition(0.into())
        .unwrap()
        .claim_next()
        .unwrap()
        .run();
    assert!(!work.cancel_sync().unwrap());
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(Arc::strong_count(&queue), 1);
}

/// Positive delays use monotonic time; both overflow layers preserve existing work.
///
/// # Panics
/// Panics on accepted overflow, early deadline, replacement on failure or leaked Queue.
#[def_test]
fn relative_delay_checks_range_without_replacing_pending() {
    let queue = queue_for(&[0], 1);
    let work = pin!(DelayedWork::new(|| {}));
    for delay in [TimeSpan::MAX, TimeSpan::from_nanos(u64::MAX)] {
        assert_eq!(
            // SAFETY: pinned idle storage remains live; out-of-range deadlines reject.
            unsafe { work.as_ref().queue_after_on(&queue, 0.into(), delay) },
            Err(ArmError::Timer(kerrno::KError::InvalidInput))
        );
        assert!(!work.work_for_test().state.lock().has_pending());
    }
    let delay = TimeSpan::from_nanos(u64::MAX / 2);
    let before = khal::time::monotonic_time().checked_add(delay).unwrap();
    // SAFETY: stable shared storage outlives the final synchronized cancellation.
    unsafe { work.as_ref().queue_after_on(&queue, 0.into(), delay) }.unwrap();
    let after = khal::time::monotonic_time().checked_add(delay).unwrap();
    let deadline_ns = work
        .work_for_test()
        .state
        .lock()
        .delayed_deadline_ns()
        .unwrap();
    let deadline = MonotonicInstant::from_span_since_origin(TimeSpan::from_nanos(deadline_ns));
    assert!(before <= deadline && deadline <= after);
    assert_eq!(
        // SAFETY: conversion overflow must leave the retained timer unchanged.
        unsafe {
            work.as_ref()
                .queue_after_on(&queue, 0.into(), TimeSpan::MAX)
        },
        Err(ArmError::Timer(kerrno::KError::InvalidInput))
    );
    assert_eq!(
        // SAFETY: ordinary admission must reject the same pending delayed occurrence.
        unsafe {
            work.as_ref()
                .queue_after_on(&queue, 0.into(), TimeSpan::ZERO)
        },
        Err(ArmError::Admission(SubmitError::AlreadyQueued))
    );
    assert_eq!(
        work.work_for_test().state.lock().delayed_deadline_ns(),
        Some(deadline_ns)
    );
    assert!(work.cancel_sync().unwrap());
    assert_eq!(Arc::strong_count(&queue), 1);
}

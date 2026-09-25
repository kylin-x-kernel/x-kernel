// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Protected flush handoff tests, not a complete public wait.
//! Boot timer service stays installed; each owned timer is drained before release.

use alloc::sync::Arc;
use core::{
    pin::pin,
    sync::atomic::{AtomicUsize, Ordering},
};

use kcpu_id_map::KCpuMaskExt;
use ktime_types::TimeSpan;
use unittest::def_test;

use super::{DelayedWork, FlushError, SubmitError, WaitError};
use crate::{queue::tests::queue_for, work::observers::CompletionObserver};

fn later() -> TimeSpan {
    TimeSpan::from_nanos(u64::MAX / 2)
}

/// Atomic capture expedites one timer without inline execution or extra acceptance.
///
/// # Panics
/// Panics on early completion, missing readiness, duplicate execution or incomplete drain.
#[def_test]
fn flush_capture_expedites_and_observes_execution() {
    let queue = queue_for(&[0], 1);
    let partition = queue.partition(0.into()).unwrap();
    let calls = AtomicUsize::new(0);
    let work = pin!(DelayedWork::new(|| {
        calls.fetch_add(1, Ordering::Relaxed);
    }));
    // SAFETY: pinned storage and capture outlive explicit execution and timer drain.
    unsafe { work.as_ref().queue_after_on(&queue, 0.into(), later()) }.unwrap();
    let (receipt, token) = CompletionObserver::prepare();
    let mut prepared = alloc::vec![token];
    {
        let _irq = kspin::NoPreemptIrqSave::new();
        assert!(
            // SAFETY: modeled flush preflight, matching retained pinned timer/Work.
            unsafe { partition.capture_and_expedite(&work.work, &work.timer, &mut prepared) }
                .unwrap()
        );
        assert!(work.work.state.lock().pending_is_ready());
        assert!(!receipt.is_completed());
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }
    partition.claim_next().unwrap().run();
    assert!(receipt.is_completed());
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert!(partition.claim_next().is_none());
    assert!(!work.cancel_sync().unwrap());
    queue.flush().unwrap();
}

/// Capacity retry has no effects; captured predecessor and successor remain ordered.
///
/// # Panics
/// Panics on timer mutation before preparation, overlap or incomplete observations.
#[def_test]
fn flush_capture_rechecks_capacity_and_respects_predecessor() {
    let queue = queue_for(&[0], 2);
    let partition = queue.partition(0.into()).unwrap();
    let work = pin!(DelayedWork::new(|| {}));
    // SAFETY: stable shared storage spans both controlled executions and timer drain.
    unsafe {
        work.as_ref()
            .queue_after_on(&queue, 0.into(), TimeSpan::ZERO)
    }
    .unwrap();
    let running = partition.claim_next().unwrap();
    // SAFETY: successor uses the same retained pinned object and owner.
    unsafe { work.as_ref().queue_after_on(&queue, 0.into(), later()) }.unwrap();
    let deadline = work.work.state.lock().delayed_deadline_ns();
    let (first, token) = CompletionObserver::prepare();
    let mut prepared = alloc::vec![token];
    assert!(
        // SAFETY: retained pinned pair; modeled preflight precedes this protected check.
        !unsafe { partition.capture_and_expedite(&work.work, &work.timer, &mut prepared) }.unwrap()
    );
    assert_eq!(prepared.len(), 1);
    assert_eq!(work.work.state.lock().delayed_deadline_ns(), deadline);
    assert_eq!(work.work.observer_count(), 2);
    let (second, token) = CompletionObserver::prepare();
    prepared.push(token);
    assert!(
        // SAFETY: same retained pair and now sufficient preallocated receipt capacity.
        unsafe { partition.capture_and_expedite(&work.work, &work.timer, &mut prepared) }.unwrap()
    );
    assert!(partition.claim_next().is_none());
    running.run();
    assert_ne!(first.is_completed(), second.is_completed());
    partition.claim_next().unwrap().run();
    assert!(first.is_completed() && second.is_completed());
    assert!(!work.cancel_sync().unwrap());
}

/// Withdrawal ends captured ordinary scope without following or deleting the new timer.
///
/// # Panics
/// Panics on a receipt following a future deadline or losing retained future work.
#[def_test]
fn flush_capture_ends_on_retime_without_canceling_future_work() {
    let queue = queue_for(&[0], 1);
    let partition = queue.partition(0.into()).unwrap();
    let work = pin!(DelayedWork::new(|| {}));
    // SAFETY: retained pinned storage spans initial timer, withdrawal and final drain.
    unsafe { work.as_ref().queue_after_on(&queue, 0.into(), later()) }.unwrap();
    let (receipt, token) = CompletionObserver::prepare();
    let mut prepared = alloc::vec![token];
    assert!(
        // SAFETY: modeled preflight and matching retained timer/Work pair.
        unsafe { partition.capture_and_expedite(&work.work, &work.timer, &mut prepared) }.unwrap()
    );
    assert!(!receipt.is_completed());
    // SAFETY: retime keeps the same external object live until final synchronized drain.
    unsafe {
        work.as_ref()
            .modify_after(&queue, Some(0.into()), later(), || unreachable!())
    }
    .unwrap();
    assert!(receipt.is_completed());
    assert!(partition.claim_next().is_none());
    assert!(work.work.state.lock().delayed_deadline_ns().is_some());
    assert_eq!(work.timer.try_cancel(), ktask::CancelResult::Removed);
    assert!(work.cancel_sync().unwrap());
}

/// Stale ownership and closure leave receipts and existing timer placement unchanged.
///
/// # Panics
/// Panics on fabricated idle work, stale-route mutation or partial failure effects.
#[def_test]
fn flush_capture_rejections_preserve_prepared_receipts() {
    let queue = queue_for(&[0, 1], 1);
    let owner = queue.partition(0.into()).unwrap();
    let other = queue.partition(1.into()).unwrap();
    let work = pin!(DelayedWork::new(|| {}));
    let (receipt, token) = CompletionObserver::prepare();
    let mut prepared = alloc::vec![token];
    assert!(
        // SAFETY: pinned idle storage is not published by this stale-owner attempt.
        !unsafe { owner.capture_and_expedite(&work.work, &work.timer, &mut prepared) }.unwrap()
    );
    // SAFETY: prepared object remains pinned through final synchronized cancellation.
    unsafe { work.as_ref().queue_after_on(&queue, 0.into(), later()) }.unwrap();
    let before = work.work.state.lock().delayed_deadline_ns();
    assert!(
        // SAFETY: wrong partition must reject this still retained matching pair.
        !unsafe { other.capture_and_expedite(&work.work, &work.timer, &mut prepared) }.unwrap()
    );
    // Deliberately model teardown misuse to check rejection without losing
    // the caller-retained timer. Queue drain excludes timer-only placement.
    queue.close_and_drain().unwrap();
    assert_eq!(
        // SAFETY: modeled preflight, pinned Work and prepared timer remain retained.
        unsafe { owner.capture_and_expedite(&work.work, &work.timer, &mut prepared) },
        Err(SubmitError::Closed)
    );
    assert_eq!(prepared.len(), 1);
    assert!(!receipt.is_completed());
    assert_eq!(work.work.state.lock().delayed_deadline_ns(), before);
    assert!(work.cancel_sync().unwrap());
}

/// Running timer delivery postpones capture without losing placement or accounting.
///
/// # Panics
/// Panics on premature capture, leaked color charge, or lost concurrent retiming.
#[def_test]
fn flush_capture_retries_running_timer_before_establishing_scope() {
    let queue = queue_for(&[0], 1);
    let partition = queue.partition(0.into()).unwrap();
    let work = pin!(DelayedWork::new(|| {}));
    // SAFETY: pinned storage remains retained through cancellation and timer drain.
    unsafe { work.as_ref().queue_after_on(&queue, 0.into(), later()) }.unwrap();
    let (receipt, token) = CompletionObserver::prepare();
    let mut prepared = alloc::vec![token];
    let deadline = work.work.state.lock().delayed_deadline_ns();
    assert!(
        // SAFETY: controlled provider model leaves the matching physical timer
        // untouched, exactly as a Running result does; storage stays pinned.
        !unsafe {
            partition.capture_and_expedite_with(&work.work, &mut prepared, || {
                ktask::CancelResult::Running
            })
        }
        .unwrap()
    );
    assert_eq!(prepared.len(), 1);
    assert!(!receipt.is_completed());
    assert_eq!(work.work.state.lock().delayed_deadline_ns(), deadline);
    assert!(partition.claim_next().is_none());
    // A leaked speculative charge would hang this otherwise empty queue flush.
    queue.flush().unwrap();
    // SAFETY: modification between attempts is permitted; the next attempt must
    // observe current placement rather than a saved pre-retry snapshot.
    unsafe {
        work.as_ref()
            .modify_after(&queue, Some(0.into()), TimeSpan::ZERO, || unreachable!())
    }
    .unwrap();
    assert!(
        // SAFETY: same retained matching pair with a controlled running result.
        !unsafe {
            partition.capture_and_expedite_with(&work.work, &mut prepared, || {
                ktask::CancelResult::Running
            })
        }
        .unwrap()
    );
    assert_eq!(prepared.len(), 1);
    assert!(work.work.state.lock().pending_is_ready());
    let running = partition.claim_next().unwrap();
    assert!(
        // SAFETY: same model also tests the running-only Work capture path.
        !unsafe {
            partition.capture_and_expedite_with(&work.work, &mut prepared, || {
                ktask::CancelResult::Running
            })
        }
        .unwrap()
    );
    assert_eq!(prepared.len(), 1);
    assert!(
        // SAFETY: actual provider is now quiescent; retained Work spans completion.
        unsafe { partition.capture_and_expedite(&work.work, &work.timer, &mut prepared) }.unwrap()
    );
    assert!(prepared.is_empty());
    assert!(!receipt.is_completed());
    running.run();
    assert!(receipt.is_completed());
    assert!(!work.cancel_sync().unwrap());
    queue.flush().unwrap();
}

/// Complete flush expedites and parks until the ordinary callback has completed.
///
/// # Panics
/// Panics on early return, inline execution, leaked disable or missing expedition.
#[def_test]
fn delayed_flush_expedites_and_waits_without_disabling() {
    let queue = queue_for(&[0], 1);
    let partition = queue.partition(0.into()).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let work = Arc::pin(DelayedWork::new({
        let calls = calls.clone();
        move || {
            calls.fetch_add(1, Ordering::Relaxed);
        }
    }));
    // SAFETY: test retains pinned storage through waiter join and final drain.
    unsafe { work.as_ref().queue_after_on(&queue, 0.into(), later()) }.unwrap();
    let flusher = ktask::spawn({
        let work = work.clone();
        move || assert_eq!(work.flush(), Ok(()))
    });
    while flusher.state() != ktask::TaskState::Blocked {
        ktask::yield_now();
    }
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    assert!(!work.work.state.lock().is_disabled());
    assert_eq!(work.timer.try_cancel(), ktask::CancelResult::Inactive);
    partition.claim_next().unwrap().run();
    assert_eq!(flusher.join(), 0);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(work.flush(), Ok(()));
    assert!(!work.cancel_sync().unwrap());
    queue.flush().unwrap();
}

/// Retiming ends a captured queueing without following or deleting its new timer.
///
/// # Panics
/// Panics on missing withdrawal completion, canceled future timer or lost resources.
#[def_test]
fn delayed_flush_returns_after_withdrawal_and_preserves_future_timer() {
    let queue = queue_for(&[0], 1);
    let work = Arc::pin(DelayedWork::new(|| {}));
    // SAFETY: retained pinned storage spans retime, waiter join and final drain.
    unsafe { work.as_ref().queue_after_on(&queue, 0.into(), later()) }.unwrap();
    let flusher = ktask::spawn({
        let work = work.clone();
        move || assert_eq!(work.flush(), Ok(()))
    });
    while flusher.state() != ktask::TaskState::Blocked {
        ktask::yield_now();
    }
    // SAFETY: concurrent retime reuses the same retained pinned Work and queue.
    unsafe {
        work.as_ref()
            .modify_after(&queue, Some(0.into()), later(), || unreachable!())
    }
    .unwrap();
    assert_eq!(flusher.join(), 0);
    assert!(queue.partition(0.into()).unwrap().claim_next().is_none());
    assert_eq!(work.timer.try_cancel(), ktask::CancelResult::Removed);
    assert!(work.cancel_sync().unwrap());
    assert_eq!(Arc::strong_count(&queue), 1);
}

/// Context and closed-transfer errors do not remove existing timer membership.
///
/// # Panics
/// Panics on preflight effects, timer loss or releasing another disable owner.
#[def_test]
fn delayed_flush_errors_preserve_timer_and_external_disable() {
    let queue = queue_for(&[0], 1);
    let work = pin!(DelayedWork::new(|| {}));
    {
        let _irq = kspin::NoPreemptIrqSave::new();
        assert_eq!(
            work.flush(),
            Err(FlushError::Wait(WaitError::InvalidContext))
        );
    }
    // SAFETY: pinned storage is retained until the explicit stop below.
    unsafe { work.as_ref().queue_after_on(&queue, 0.into(), later()) }.unwrap();
    {
        let _irq = kspin::NoPreemptIrqSave::new();
        assert_eq!(
            work.flush(),
            Err(FlushError::Wait(WaitError::InvalidContext))
        );
    }
    // Model caller teardown misuse; flush must not compound it by losing the timer.
    queue.close_and_drain().unwrap();
    assert_eq!(work.flush(), Err(FlushError::Transfer(SubmitError::Closed)));
    assert_eq!(work.timer.try_cancel(), ktask::CancelResult::Removed);
    let (guard, revoked) = work.disable();
    assert!(revoked);
    assert_eq!(work.flush(), Ok(()));
    assert!(work.work.state.lock().is_disabled());
    drop(guard);
    assert!(!work.cancel_sync().unwrap());
}

fn prepare_on(cpu: usize, run: impl FnOnce() + Send + 'static) -> ktask::KtaskRef {
    let task = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread(
        run,
        "delayed_flush_probe".into(),
        64 * 1024,
    ));
    task.set_cpumask(ktask::KCpuMask::one_shot_logical(cpu.into()));
    task
}

/// Actual remote IRQ tail cannot be mistaken for completed timer delivery.
///
/// # Panics
/// Panics on early capture, incorrect CPU placement, lost readiness or failed drain.
#[def_test]
fn delayed_flush_retries_actual_remote_timer_tail() {
    if kcpu_id_map::nr_cpus() < 2 {
        return unittest::TestResult::Ignored;
    }
    let controller = prepare_on(0, || {
        let queue = queue_for(&[0], 1);
        let partition = queue.partition(0.into()).unwrap();
        let work = Arc::pin(DelayedWork::new(|| {}));
        let gate = Arc::new(AtomicUsize::new(0));
        *work.timer.handler().delivery_tail.lock() = Some(gate.clone());
        let armer = prepare_on(1, {
            let (work, queue) = (work.clone(), queue.clone());
            move || {
                assert_eq!(khal::percpu::this_cpu_id().as_usize(), 1);
                // SAFETY: controller retains pinned storage until gate release,
                // all task joins and synchronized timer/ordinary drain.
                unsafe {
                    work.as_ref()
                        .arm_on(&queue, 0.into(), khal::time::monotonic_time())
                }
                .unwrap();
            }
        });
        ktask::activate_task(&armer);
        while gate.load(Ordering::Acquire) != 1 {
            ktask::yield_now();
        }
        let (receipt, token) = CompletionObserver::prepare();
        let mut prepared = alloc::vec![token];
        // Release the IRQ even when the following assertions detect a regression.
        // Capture must return without waiting while the remote tail holds Running.
        // SAFETY: controller retains the matching pinned pair and prepared token;
        // sleepable preflight is modeled for this nonwaiting transaction probe.
        let captured =
            unsafe { partition.capture_and_expedite(&work.work, &work.timer, &mut prepared) };
        gate.store(2, Ordering::Release);
        assert_eq!(captured, Ok(false));
        assert_eq!(prepared.len(), 1);
        assert!(!receipt.is_completed());
        assert_eq!(armer.join(), 0);
        let flusher = prepare_on(0, {
            let work = work.clone();
            move || assert_eq!(work.flush(), Ok(()))
        });
        ktask::activate_task(&flusher);
        while flusher.state() != ktask::TaskState::Blocked {
            ktask::yield_now();
        }
        partition.claim_next().unwrap().run();
        assert_eq!(flusher.join(), 0);
        assert!(!work.cancel_sync().unwrap());
        *work.timer.handler().delivery_tail.lock() = None;
    });
    ktask::activate_task(&controller);
    assert_eq!(controller.join(), 0);
}

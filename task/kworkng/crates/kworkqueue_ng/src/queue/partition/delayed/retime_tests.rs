// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Real prepared timers with controlled ordinary claims. The boot timer service
//! stays installed; every owned timer is synchronously drained before release.

use alloc::sync::Arc;
use core::{
    pin::pin,
    sync::atomic::{AtomicUsize, Ordering},
};

use ktime_types::{MonotonicInstant, TimeSpan};
use unittest::def_test;

use super::{ArmError, SubmitError};
use crate::{
    queue::tests::queue_for,
    work::{Work, delayed::DelayedWork, observers::CompletionObserver},
};

fn deadline(ns: u64) -> MonotonicInstant {
    MonotonicInstant::from_span_since_origin(TimeSpan::from_nanos(ns))
}

/// Ready withdrawal returns allowance and ends old observations, not the new timer.
///
/// # Panics
/// Panics on lost refill, premature resources release, wrong scope or duplicate callback.
#[def_test]
fn ready_withdrawal_releases_allowance_and_old_scope() {
    let queue = queue_for(&[0], 1);
    let partition = queue.partition(0.into()).unwrap();
    let calls = AtomicUsize::new(0);
    let work = pin!(DelayedWork::new(|| {
        calls.fetch_add(1, Ordering::Relaxed);
    }));
    let next = pin!(Work::with_callback(|| {}));
    // SAFETY: both pinned objects and captures remain live through execution/drain below.
    unsafe {
        work.as_ref()
            .queue_after_on(&queue, 0.into(), TimeSpan::ZERO)
    }
    .unwrap();
    // SAFETY: both pinned objects and captures remain live through execution/drain below.
    unsafe { queue.submit(&next, Some(0.into()), || unreachable!()) }.unwrap();
    let (old, token) = CompletionObserver::prepare();
    work.work_for_test()
        .capture(&mut alloc::vec![token])
        .unwrap();
    let (batch, token) = CompletionObserver::prepare();
    assert!(
        partition
            .observe_batch(queue.current_color(), token)
            .is_ok()
    );
    // SAFETY: the same Work and prepared timer stay pinned through synchronized drain.
    assert!(unsafe { work.as_ref().retime_pending_at(deadline(u64::MAX)) }.unwrap());
    assert!(old.is_completed());
    assert!(!batch.is_completed());
    assert!(next.state.lock().pending_is_ready());
    assert_eq!(partition.state.lock().active, 1);
    partition.claim_next().unwrap().run();
    assert!(batch.is_completed());
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    assert_eq!(Arc::strong_count(&queue), 2);
    queue.flush().unwrap();
    let (new, token) = CompletionObserver::prepare();
    work.work_for_test()
        .capture(&mut alloc::vec![token])
        .unwrap();
    assert!(
        // SAFETY: Work stays pinned and retained until the synchronized drain below.
        unsafe {
            work.as_ref()
                .retime_pending_at(khal::time::monotonic_time())
        }
        .unwrap()
    );
    while !work.work_for_test().state.lock().pending_is_ready() {
        ktask::yield_now();
    }
    assert!(!new.is_completed());
    partition.claim_next().unwrap().run();
    assert!(!work.cancel_sync().unwrap());
    assert!(new.is_completed());
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(Arc::strong_count(&queue), 1);
}

/// Withdrawing a waiting successor cannot release its predecessor's allowance.
///
/// # Panics
/// Panics on early running completion, lost color retirement or overlapping execution.
#[def_test]
fn waiting_successor_withdrawal_keeps_running_occupied() {
    let queue = queue_for(&[0], 2);
    let partition = queue.partition(0.into()).unwrap();
    let work = pin!(DelayedWork::new(|| {}));
    // SAFETY: pinned storage spans predecessor execution and successor cancellation.
    unsafe {
        work.as_ref()
            .queue_after_on(&queue, 0.into(), TimeSpan::ZERO)
    }
    .unwrap();
    let running = partition.claim_next().unwrap();
    queue.publish_color(1); // Fresh test color, never reuse of a live batch.
    // SAFETY: same retained storage and owner for the unclaimed successor.
    unsafe {
        work.as_ref()
            .queue_after_on(&queue, 0.into(), TimeSpan::ZERO)
    }
    .unwrap();
    let (one, a) = CompletionObserver::prepare();
    let (two, b) = CompletionObserver::prepare();
    work.work_for_test()
        .capture(&mut alloc::vec![a, b])
        .unwrap();
    let (successor, token) = CompletionObserver::prepare();
    assert!(partition.observe_batch(1, token).is_ok());
    // SAFETY: future timer retains the same pinned Work until explicit final cancel.
    assert!(unsafe { work.as_ref().retime_pending_at(deadline(u64::MAX)) }.unwrap());
    assert!(successor.is_completed());
    assert_ne!(one.is_completed(), two.is_completed());
    assert_eq!(partition.state.lock().active, 1);
    assert!(partition.claim_next().is_none());
    running.run();
    assert!(one.is_completed() && two.is_completed());
    assert_eq!(partition.state.lock().active, 0);
    assert!(work.cancel_sync().unwrap());
    assert_eq!(Arc::strong_count(&queue), 1);
}

/// Timer-to-timer adjustment keeps its observers and retains the old setting on error.
///
/// # Panics
/// Panics on premature observer completion, lost deadline or revival of idle work.
#[def_test]
fn timer_retime_preserves_observers_and_rejected_setting() {
    let queue = queue_for(&[0], 1);
    let work = pin!(DelayedWork::new(|| {}));
    // SAFETY: pinned idle storage is not published by pending-only retime.
    assert!(!unsafe { work.as_ref().retime_pending_at(deadline(u64::MAX)) }.unwrap());
    // SAFETY: stable shared storage remains live through explicit delayed cancellation.
    unsafe { work.as_ref().arm_on(&queue, 0.into(), deadline(u64::MAX)) }.unwrap();
    let (receipt, token) = CompletionObserver::prepare();
    work.work_for_test()
        .capture(&mut alloc::vec![token])
        .unwrap();
    // SAFETY: both earlier and later deadlines reuse the retained prepared timer.
    assert!(unsafe { work.as_ref().retime_pending_at(deadline(u64::MAX / 2)) }.unwrap());
    assert!(!receipt.is_completed());
    let invalid = MonotonicInstant::from_span_since_origin(TimeSpan::MAX);
    assert_eq!(
        // SAFETY: invalid deadline rejects without replacing the retained timer.
        unsafe { work.as_ref().retime_pending_at(invalid) },
        Err(ArmError::Timer(kerrno::KError::InvalidInput))
    );
    assert_eq!(
        work.work_for_test().state.lock().delayed_deadline_ns(),
        Some(u64::MAX / 2)
    );
    // SAFETY: stable timer storage remains live through the final drain.
    assert!(unsafe { work.as_ref().retime_pending_at(deadline(u64::MAX)) }.unwrap());
    assert!(!receipt.is_completed());
    assert!(work.cancel_sync().unwrap());
    assert!(receipt.is_completed());
}

/// Failed withdrawal preserves FIFO and accounting; a claimed target is not modified.
///
/// # Panics
/// Panics on changed FIFO, early receipt, unexpected timer or incomplete drain.
#[def_test]
fn rejected_withdrawal_and_claim_leave_old_queueing_intact() {
    let queue = queue_for(&[0], 1);
    let partition = queue.partition(0.into()).unwrap();
    let sequence = AtomicUsize::new(0);
    let work = pin!(DelayedWork::new(|| {
        assert_eq!(sequence.fetch_add(1, Ordering::Relaxed), 0);
    }));
    let next = pin!(Work::with_callback(|| {
        assert_eq!(sequence.fetch_add(1, Ordering::Relaxed), 1);
    }));
    // SAFETY: pinned objects and captures outlive both manual executions and timer drain.
    unsafe {
        work.as_ref()
            .queue_after_on(&queue, 0.into(), TimeSpan::ZERO)
    }
    .unwrap();
    // SAFETY: pinned objects and captures outlive both manual executions and timer drain.
    unsafe { queue.submit(&next, Some(0.into()), || unreachable!()) }.unwrap();
    let (receipt, token) = CompletionObserver::prepare();
    work.work_for_test()
        .capture(&mut alloc::vec![token])
        .unwrap();
    assert_eq!(
        // SAFETY: invalid programming cannot withdraw this retained ready Work.
        unsafe {
            work.as_ref()
                .retime_pending_at(MonotonicInstant::from_span_since_origin(TimeSpan::MAX))
        },
        Err(ArmError::Timer(kerrno::KError::InvalidInput))
    );
    queue.close_admission();
    assert_eq!(
        // SAFETY: closure rejects without touching the existing queueing.
        unsafe { work.as_ref().retime_pending_at(deadline(u64::MAX)) },
        Err(ArmError::Admission(SubmitError::Closed))
    );
    assert!(!receipt.is_completed());
    let running = partition.claim_next().unwrap();
    // SAFETY: pending-only retime cannot change the retained running occurrence.
    assert!(!unsafe { work.as_ref().retime_pending_at(deadline(u64::MAX)) }.unwrap());
    running.run();
    assert!(receipt.is_completed());
    partition.claim_next().unwrap().run();
    assert!(!work.cancel_sync().unwrap());
    assert_eq!(sequence.load(Ordering::Relaxed), 2);
    queue.close_and_drain().unwrap();
}

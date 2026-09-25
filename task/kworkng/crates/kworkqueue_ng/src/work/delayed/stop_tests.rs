// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Stop operations use the permanent boot timer service; each test drains its
//! prepared timer before releasing pinned storage. Ordinary claims are controlled.

use alloc::{sync::Arc, task::Wake};
use core::{
    pin::{Pin, pin},
    sync::atomic::{AtomicUsize, Ordering},
    task::Waker,
};

use ktime_types::TimeSpan;
use unittest::def_test;

use super::{ArmError, DelayedWork, SubmitError, WaitError};
use crate::{
    queue::{Queue, tests::queue_for},
    work::observers::CompletionObserver,
};

fn queue() -> Arc<Queue> {
    queue_for(&[0], 1)
}

fn later() -> TimeSpan {
    TimeSpan::from_nanos(u64::MAX / 2)
}

type TestWork = DelayedWork<fn()>;

struct RearmOnWake {
    work: Pin<Arc<TestWork>>,
    queue: Arc<Queue>,
    calls: AtomicUsize,
}

impl Wake for RearmOnWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        // SAFETY: the test retains the pinned Arc through notification and final
        // synchronized drain. This wake must run outside Work/partition locks.
        unsafe {
            self.work
                .as_ref()
                .queue_after_on(&self.queue, 0.into(), later())
        }
        .unwrap();
        self.calls.fetch_add(1, Ordering::Relaxed);
    }
}

/// A notification can rearm without an old cancellation deleting the new timer.
///
/// # Panics
/// Panics on lost rearm, locked notification, duplicate wake or incomplete drain.
#[def_test]
fn cancel_notification_rearm_preserves_new_physical_timer() {
    let queue = queue();
    let work = Arc::pin(DelayedWork::new((|| {}) as fn()));
    let wake = Arc::new(RearmOnWake {
        work: work.clone(),
        queue: queue.clone(),
        calls: AtomicUsize::new(0),
    });
    let (receipt, token) = CompletionObserver::prepare();
    let registration = receipt.register(&Waker::from(wake.clone())).unwrap();
    // SAFETY: test and wake retain pinned Work through completion and final drain.
    unsafe { work.as_ref().queue_after_on(&queue, 0.into(), later()) }.unwrap();
    work.work_for_test()
        .capture(&mut alloc::vec![token])
        .unwrap();
    assert!(work.cancel_pending());
    assert!(receipt.is_completed());
    assert_eq!(wake.calls.load(Ordering::Relaxed), 1);
    assert!(work.work_for_test().state.lock().has_pending());
    assert_eq!(work.timer.try_cancel(), ktask::CancelResult::Removed);
    assert!(work.cancel_sync().unwrap());
    drop(registration);
    drop(wake);
    assert_eq!(Arc::strong_count(&queue), 1);
}

/// Nonwaiting cancellation handles timer/ready work and allows immediate object reuse.
///
/// # Panics
/// Panics on revived cancellation, lost observer completion, duplicate callback or leaked Queue.
#[def_test]
fn cancel_pending_revokes_timer_and_ready_without_disabling() {
    let queue = queue();
    let calls = AtomicUsize::new(0);
    let work = pin!(DelayedWork::new(|| {
        calls.fetch_add(1, Ordering::Relaxed);
    }));
    assert!(!work.cancel_pending());
    for delay in [later(), TimeSpan::ZERO] {
        // SAFETY: pinned Work and borrowed capture remain live through final drain.
        unsafe { work.as_ref().queue_after_on(&queue, 0.into(), delay) }.unwrap();
        let (receipt, token) = CompletionObserver::prepare();
        work.work_for_test()
            .capture(&mut alloc::vec![token])
            .unwrap();
        {
            let _irq = kspin::NoPreemptIrqSave::new();
            assert!(work.cancel_pending());
            assert!(!work.cancel_pending());
        }
        assert!(receipt.is_completed());
        assert!(queue.partition(0.into()).unwrap().claim_next().is_none());
        assert_eq!(Arc::strong_count(&queue), 1);
    }
    // SAFETY: canceled storage remains pinned; a new acceptance is drained below.
    unsafe {
        work.as_ref()
            .queue_after_on(&queue, 0.into(), TimeSpan::ZERO)
    }
    .unwrap();
    queue
        .partition(0.into())
        .unwrap()
        .claim_next()
        .unwrap()
        .run();
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert!(!work.cancel_sync().unwrap());
    queue.flush().unwrap();
}

/// Cancel does not wait for a claimed predecessor, and removes only its successor.
///
/// # Panics
/// Panics on wrong revocation result, interrupted predecessor or duplicate execution.
#[def_test]
fn cancel_pending_keeps_claimed_predecessor() {
    let queue = queue();
    let calls = AtomicUsize::new(0);
    let work = pin!(DelayedWork::new(|| {
        calls.fetch_add(1, Ordering::Relaxed);
    }));
    // SAFETY: retained pinned storage spans controlled claim, successor and final drain.
    unsafe {
        work.as_ref()
            .queue_after_on(&queue, 0.into(), TimeSpan::ZERO)
    }
    .unwrap();
    let partition = queue.partition(0.into()).unwrap();
    let running = partition.claim_next().unwrap();
    assert!(!work.cancel_pending());
    for delay in [later(), TimeSpan::ZERO] {
        // SAFETY: same-owner successor shares the retained pinned object.
        unsafe { work.as_ref().queue_after_on(&queue, 0.into(), delay) }.unwrap();
        assert!(work.cancel_pending());
        assert!(partition.claim_next().is_none());
    }
    running.run();
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert!(!work.cancel_sync().unwrap());
    assert_eq!(Arc::strong_count(&queue), 1);
}

/// Independent nonwaiting guards block both fresh and modifying delayed requests.
///
/// # Panics
/// Panics on lost disable ownership, restored work or an incorrect revocation result.
#[def_test]
fn delayed_disable_guards_remain_independent() {
    let queue = queue();
    let work = pin!(DelayedWork::new(|| {}));
    // SAFETY: pinned storage stays live through both guards and final timer drain.
    unsafe { work.as_ref().queue_after_on(&queue, 0.into(), later()) }.unwrap();
    let (first, second) = {
        let _irq = kspin::NoPreemptIrqSave::new();
        let (first, revoked) = work.disable();
        assert!(revoked);
        let (second, revoked) = work.disable();
        assert!(!revoked);
        (first, second)
    };
    drop(first);
    for delay in [TimeSpan::ZERO, later()] {
        assert_eq!(
            // SAFETY: retained pinned storage and initialized timer service outlive rejection.
            unsafe {
                work.as_ref()
                    .modify_after(&queue, Some(0.into()), delay, || unreachable!())
            },
            Err(ArmError::Admission(SubmitError::Disabled))
        );
    }
    drop(second);
    assert!(!work.work_for_test().state.lock().has_pending());
    // SAFETY: the last guard permits reuse of still-pinned prepared storage.
    unsafe { work.as_ref().queue_after_on(&queue, 0.into(), later()) }.unwrap();
    assert!(work.cancel_sync().unwrap());
    assert_eq!(Arc::strong_count(&queue), 1);
}

/// Synchronous disable validates context first and retains its disable after timer drain.
///
/// # Panics
/// Panics on pre-effect mutation, lost persistent disable or failed reuse/drain.
#[def_test]
fn delayed_sync_disable_preserves_context_and_guard_contracts() {
    let queue = queue();
    let work = pin!(DelayedWork::new(|| {}));
    // SAFETY: stable shared storage remains pinned through the synchronized stop below.
    unsafe { work.as_ref().queue_after_on(&queue, 0.into(), later()) }.unwrap();
    let deadline = work.work_for_test().state.lock().delayed_deadline_ns();
    {
        let _irq = kspin::NoPreemptIrqSave::new();
        assert!(matches!(
            work.disable_sync(),
            Err(WaitError::InvalidContext)
        ));
    }
    assert_eq!(
        work.work_for_test().state.lock().delayed_deadline_ns(),
        deadline
    );
    let (guard, revoked) = work.disable_sync().unwrap();
    assert!(revoked);
    assert!(!work.cancel_sync().unwrap());
    assert_eq!(
        // SAFETY: pinned storage remains disabled by guard, not by the nested cancel.
        unsafe {
            work.as_ref()
                .queue_after_on(&queue, 0.into(), TimeSpan::ZERO)
        },
        Err(ArmError::Admission(SubmitError::Disabled))
    );
    drop(guard);
    // SAFETY: same prepared storage is retained through its final synchronized drain.
    unsafe { work.as_ref().queue_after_on(&queue, 0.into(), later()) }.unwrap();
    assert!(work.cancel_sync().unwrap());
    assert_eq!(Arc::strong_count(&queue), 1);
}

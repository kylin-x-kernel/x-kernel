// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Boot scheduler/timer services stay installed for the whole kernel test run.
//! Tests add no global hooks; each attempt owns and drains its deadline timer.

use alloc::sync::Arc;
use core::{
    pin::pin,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use unittest::def_test;

use super::*;
use crate::{CpuPolicy, QueueConfig, work::Work};

fn queue(max_active: usize) -> Arc<Queue> {
    Queue::new(QueueConfig::new(&[0.into()], CpuPolicy::CurrentCpu, max_active).unwrap())
}

fn wait_blocked(task: &ktask::KtaskRef) {
    while task.state() != ktask::TaskState::Blocked {
        ktask::yield_now();
    }
}

/// Ready reservations, waiting work and IRQ submissions survive a dispatch pause.
#[def_test]
fn pending_admission_and_cancellation_survive_freeze() {
    let queue = queue(1);
    let calls = AtomicUsize::new(0);
    let first = pin!(Work::with_callback(|| {
        assert_eq!(calls.fetch_add(1, Ordering::Relaxed), 0);
    }));
    let cancelled = pin!(Work::with_callback(|| panic!("cancelled work ran")));
    let later = pin!(Work::with_callback(|| {
        assert_eq!(calls.fetch_add(1, Ordering::Relaxed), 1);
    }));
    // SAFETY: pinned work/captures survive execution or cancellation below.
    unsafe { queue.submit(&first, None, || 0.into()) }.unwrap();
    // SAFETY: the pinned cancelled work remains live through synchronous cancellation.
    unsafe { queue.submit(&cancelled, None, || 0.into()) }.unwrap();
    let cancellation = QueueFreezeCancellation::new();
    let frozen = queue.freeze(TimeSpan::ZERO, &cancellation).unwrap();
    {
        let _irq = kspin::NoPreemptIrqSave::new();
        // SAFETY: prepared pinned work survives its later execution and drain.
        unsafe { queue.submit(&later, None, || 0.into()) }.unwrap();
    }
    assert!(queue.claim_with_demand(0.into()).0.is_none());
    assert!(cancelled.cancel_pending());
    cancellation.cancel();
    assert!(queue.claim_with_demand(0.into()).0.is_none());
    assert_eq!(
        queue
            .freeze(TimeSpan::ZERO, &QueueFreezeCancellation::new())
            .err(),
        Some(QueueFreezeError::Busy)
    );
    drop(frozen);
    queue.claim_with_demand(0.into()).0.unwrap().run();
    queue.claim_with_demand(0.into()).0.unwrap().run();
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    queue.close_and_drain().unwrap();
}

/// A claim predating the cutoff is joined even before callback entry.
#[def_test]
fn waits_for_claim_and_preserves_callback_requeue() {
    let queue = queue(1);
    let work = pin!(Work::with_callback(|| {}));
    // SAFETY: pinned work survives both executions and the joined freezer.
    unsafe { queue.submit(&work, None, || 0.into()) }.unwrap();
    let execution = queue.claim_with_demand(0.into()).0.unwrap();
    let frozen = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let task = ktask::spawn({
        let (queue, frozen, release) = (queue.clone(), frozen.clone(), release.clone());
        move || {
            let _guard = queue
                .freeze(TimeSpan::from_secs(60), &QueueFreezeCancellation::new())
                .unwrap();
            frozen.store(true, Ordering::Release);
            while !release.load(Ordering::Acquire) {
                ktask::yield_now();
            }
        }
    });
    wait_blocked(&task);
    assert!(!frozen.load(Ordering::Acquire));
    execution.run_with(|| {
        // SAFETY: this successor uses the same pinned work, retained through drain.
        unsafe { queue.submit(&work, None, || 0.into()) }.unwrap();
        assert!(queue.claim_with_demand(0.into()).0.is_none());
    });
    while !frozen.load(Ordering::Acquire) {
        ktask::yield_now();
    }
    assert!(queue.claim_with_demand(0.into()).0.is_none());
    release.store(true, Ordering::Release);
    assert_eq!(task.join(), 0);
    queue.claim_with_demand(0.into()).0.unwrap().run();
    queue.close_and_drain().unwrap();
}

/// Deadline expiry rolls back a pause without consuming pending work.
#[def_test]
fn timeout_reopens_every_partition() {
    let queue = Queue::new(
        QueueConfig::validate(&[0.into(), 1.into()], CpuPolicy::CurrentCpu, 2, |_| true).unwrap(),
    );
    let running = pin!(Work::with_callback(|| {}));
    let later = pin!(Work::with_callback(|| {}));
    // SAFETY: both pinned fixtures are executed before leaving this scope.
    unsafe { queue.submit(&running, Some(0.into()), || unreachable!()) }.unwrap();
    // SAFETY: the second virtual partition is manually driven with retained storage.
    unsafe { queue.submit(&later, Some(1.into()), || unreachable!()) }.unwrap();
    let execution = queue.claim_with_demand(0.into()).0.unwrap();
    for timeout in [TimeSpan::ZERO, TimeSpan::from_millis(2)] {
        assert_eq!(
            queue.freeze(timeout, &QueueFreezeCancellation::new()).err(),
            Some(QueueFreezeError::TimedOut)
        );
        assert!(!*queue.is_freezing.lock());
    }
    queue.claim_with_demand(1.into()).0.unwrap().run();
    execution.run();
    drop(
        queue
            .freeze(TimeSpan::ZERO, &QueueFreezeCancellation::new())
            .unwrap(),
    );
    queue.close_and_drain().unwrap();
}

/// IRQ cancellation wakes a waiting coordinator and preserves subsequent claims.
#[def_test]
fn cancellation_rolls_back_waiting_freeze() {
    let queue = queue(2);
    let first = pin!(Work::with_callback(|| {}));
    let pending = pin!(Work::with_callback(|| {}));
    // SAFETY: pinned fixtures survive the joined attempt and final executions.
    unsafe { queue.submit(&first, None, || 0.into()) }.unwrap();
    // SAFETY: this ready fixture is retained until it is run below.
    unsafe { queue.submit(&pending, None, || 0.into()) }.unwrap();
    let execution = queue.claim_with_demand(0.into()).0.unwrap();
    let cancellation = QueueFreezeCancellation::new();
    let task = ktask::spawn({
        let (queue, cancellation) = (queue.clone(), cancellation.clone());
        move || {
            assert_eq!(
                queue.freeze(TimeSpan::from_secs(60), &cancellation).err(),
                Some(QueueFreezeError::Cancelled)
            )
        }
    });
    wait_blocked(&task);
    {
        let _irq = kspin::NoPreemptIrqSave::new();
        cancellation.cancel();
    }
    assert_eq!(task.join(), 0);
    queue.claim_with_demand(0.into()).0.unwrap().run();
    execution.run();
    queue.close_and_drain().unwrap();
}

/// Context and cancelled/closed attempts leave dispatch and admission unchanged.
#[def_test]
fn preflight_failures_have_no_dispatch_effects() {
    let queue = queue(1);
    let cancellation = QueueFreezeCancellation::new();
    assert_eq!(
        queue.freeze(TimeSpan::MAX, &cancellation).err(),
        Some(QueueFreezeError::InvalidTimeout)
    );
    assert!(matches!(
        queue.freeze(TimeSpan::from_secs(u64::MAX / 2), &cancellation),
        Err(QueueFreezeError::Timer(_))
    ));
    {
        let _irq = kspin::NoPreemptIrqSave::new();
        assert_eq!(
            queue.freeze(TimeSpan::ZERO, &cancellation).err(),
            Some(QueueFreezeError::InvalidContext)
        );
    }
    cancellation.cancel();
    assert_eq!(
        queue.freeze(TimeSpan::ZERO, &cancellation).err(),
        Some(QueueFreezeError::Cancelled)
    );
    assert!(!*queue.is_freezing.lock());
    queue.close_and_drain().unwrap();
    assert_eq!(
        queue
            .freeze(TimeSpan::ZERO, &QueueFreezeCancellation::new())
            .err(),
        Some(QueueFreezeError::Closed)
    );
}

/// Returning from the callback is insufficient while payload destruction is active.
#[def_test]
fn freeze_joins_occurrence_resource_cleanup() {
    struct Cleanup {
        entered: Arc<AtomicBool>,
        release: Arc<AtomicBool>,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            self.entered.store(true, Ordering::Release);
            while !self.release.load(Ordering::Acquire) {
                ktask::yield_now();
            }
        }
    }
    let queue = queue(1);
    let entered = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let work = Arc::new(Work::with_callback(|| {}));
    let partition = queue.partition(0.into()).unwrap();
    // SAFETY: stable Arc allocation is retained through joined execution/cleanup.
    let accepted = unsafe {
        partition.accept(
            &work,
            Cleanup {
                entered: entered.clone(),
                release: release.clone(),
            },
            0,
        )
    };
    assert!(accepted.is_ok());
    let worker = ktask::spawn({
        let queue = queue.clone();
        move || queue.claim_with_demand(0.into()).0.unwrap().run()
    });
    while !entered.load(Ordering::Acquire) {
        ktask::yield_now();
    }
    assert_eq!(
        queue
            .freeze(TimeSpan::ZERO, &QueueFreezeCancellation::new())
            .err(),
        Some(QueueFreezeError::TimedOut)
    );
    release.store(true, Ordering::Release);
    assert_eq!(worker.join(), 0);
    drop(
        queue
            .freeze(TimeSpan::ZERO, &QueueFreezeCancellation::new())
            .unwrap(),
    );
    queue.close_and_drain().unwrap();
}

/// Thaw cannot reopen permanent admission closed by a concurrent shutdown.
#[def_test]
fn concurrent_shutdown_remains_closed_after_thaw() {
    let queue = queue(1);
    let work = pin!(Work::with_callback(|| {}));
    // SAFETY: pinned work remains live until the final execution and joined drain.
    unsafe { queue.submit(&work, None, || 0.into()) }.unwrap();
    let guard = queue
        .freeze(TimeSpan::ZERO, &QueueFreezeCancellation::new())
        .unwrap();
    let closer = ktask::spawn({
        let queue = queue.clone();
        move || queue.close_and_drain().unwrap()
    });
    wait_blocked(&closer);
    assert!(queue.admission_color().is_none());
    drop(guard);
    assert!(queue.admission_color().is_none());
    queue.claim_with_demand(0.into()).0.unwrap().run();
    assert_eq!(closer.join(), 0);
}

/// Timer-to-queue expiry retains ordinary work until the guard is released.
#[def_test]
fn delayed_expiration_while_frozen_is_retained() {
    let queue = queue(1);
    let calls = AtomicUsize::new(0);
    let work = pin!(Work::with_callback(|| {
        calls.fetch_add(1, Ordering::Relaxed);
    }));
    let guard = queue
        .freeze(TimeSpan::ZERO, &QueueFreezeCancellation::new())
        .unwrap();
    let partition = queue.partition(0.into()).unwrap();
    partition.install_delayed_for_test(&queue, &work, 10);
    // SAFETY: expiry uses pinned work retained through completion and final drain.
    assert_eq!(unsafe { partition.queue_due(&work, 10) }, Ok(true));
    assert!(queue.claim_with_demand(0.into()).0.is_none());
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    drop(guard);
    queue.claim_with_demand(0.into()).0.unwrap().run();
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    queue.close_and_drain().unwrap();
}

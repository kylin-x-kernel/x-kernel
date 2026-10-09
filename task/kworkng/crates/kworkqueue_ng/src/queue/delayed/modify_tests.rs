// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Prepared modification with controlled claims and permanent boot timer service.

use alloc::sync::Arc;
use core::{
    cell::Cell,
    pin::pin,
    sync::atomic::{AtomicUsize, Ordering},
};

use ktime_types::TimeSpan;
use unittest::def_test;

use super::{
    ArmError, SubmitError,
    tests::{later, queue},
};
use crate::{
    queue::config::CpuPolicy,
    work::{delayed::DelayedWork, observers::CompletionObserver},
};

/// Zero adjustment preserves waiting FIFO position and charges no second occurrence.
///
/// # Panics
/// Panics on reordered execution, duplicate accounting or failed synchronized drain.
#[def_test]
fn zero_modification_preserves_waiting_fifo() {
    let queue = queue(CpuPolicy::RoundRobin);
    let sequence = AtomicUsize::new(0);
    let blocker = pin!(DelayedWork::new(|| {
        assert_eq!(sequence.fetch_add(1, Ordering::Relaxed), 0);
    }));
    let first = pin!(DelayedWork::new(|| {
        assert_eq!(sequence.fetch_add(1, Ordering::Relaxed), 1);
    }));
    let second = pin!(DelayedWork::new(|| {
        assert_eq!(sequence.fetch_add(1, Ordering::Relaxed), 2);
    }));
    // SAFETY: all three pinned objects and shared capture remain live through
    // controlled execution, queue flush and individual timer drain below.
    unsafe {
        blocker
            .as_ref()
            .modify_after(&queue, Some(0.into()), TimeSpan::ZERO, || unreachable!())
    }
    .unwrap();
    // SAFETY: all three pinned objects and shared capture remain live through
    // controlled execution, queue flush and individual timer drain below.
    unsafe {
        first
            .as_ref()
            .modify_after(&queue, Some(0.into()), TimeSpan::ZERO, || unreachable!())
    }
    .unwrap();
    // SAFETY: all three pinned objects and shared capture remain live through
    // controlled execution, queue flush and individual timer drain below.
    unsafe {
        second
            .as_ref()
            .modify_after(&queue, Some(0.into()), TimeSpan::ZERO, || unreachable!())
    }
    .unwrap();
    // SAFETY: all three pinned objects and shared capture remain live through
    // controlled execution, queue flush and individual timer drain below.
    unsafe {
        first
            .as_ref()
            .modify_after(&queue, Some(0.into()), TimeSpan::ZERO, || unreachable!())
    }
    .unwrap();
    let partition = queue.partition(0.into()).unwrap();
    assert!(!first.work_for_test().state.lock().pending_is_ready());
    for _ in 0..3 {
        partition.claim_next().unwrap().run();
    }
    assert!(partition.claim_next().is_none());
    queue.flush().unwrap();
    assert_eq!(sequence.load(Ordering::Relaxed), 3);
    assert!(!blocker.cancel_sync().unwrap());
    assert!(!first.cancel_sync().unwrap());
    assert!(!second.cancel_sync().unwrap());
    assert_eq!(Arc::strong_count(&queue), 1);
}

/// Fresh modification routes once; timer/ordinary changes reuse the same occurrence.
///
/// # Panics
/// Panics on premature completion, duplicate callback, IRQ dependency or leaked resources.
#[def_test]
fn modify_reuses_pending_and_zero_needs_no_irq() {
    let queue = queue(CpuPolicy::RoundRobin);
    let calls = AtomicUsize::new(0);
    let work = pin!(DelayedWork::new(|| {
        calls.fetch_add(1, Ordering::Relaxed);
    }));
    assert_eq!(
        // SAFETY: pinned Work and borrowed capture outlive all calls and final drain.
        unsafe {
            work.as_ref()
                .modify_after(&queue, None, later(), || panic!("RR"))
        },
        Ok(0.into())
    );
    let (receipt, token) = CompletionObserver::prepare();
    work.work_for_test()
        .capture(&mut alloc::vec![token])
        .unwrap();
    {
        let _irq = kspin::NoPreemptIrqSave::new();
        assert_eq!(
            // SAFETY: retained pinned storage, initialized timer service; no sleeping call.
            unsafe {
                work.as_ref()
                    .modify_after(&queue, None, TimeSpan::ZERO, || panic!("owner"))
            },
            Ok(0.into())
        );
        assert!(work.work_for_test().state.lock().pending_is_ready());
        assert!(!receipt.is_completed());
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        // SAFETY: repeat zero adjustment preserves the same retained ordinary occurrence.
        unsafe {
            work.as_ref()
                .modify_after(&queue, None, TimeSpan::ZERO, || panic!("owner"))
        }
        .unwrap();
    }
    // SAFETY: same prepared pair remains pinned through withdrawal and final drain.
    unsafe {
        work.as_ref()
            .modify_after(&queue, None, later(), || panic!("owner"))
    }
    .unwrap();
    assert!(receipt.is_completed());
    assert!(queue.partition(0.into()).unwrap().claim_next().is_none());
    // SAFETY: expedition retains storage through the manual ordinary execution below.
    unsafe {
        work.as_ref()
            .modify_after(&queue, None, TimeSpan::ZERO, || panic!("owner"))
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
    assert!(queue.partition(0.into()).unwrap().claim_next().is_none());
    assert_eq!(queue.select_cpu(None, || panic!("RR")), Ok(2.into()));
    assert_eq!(Arc::strong_count(&queue), 1);
}

/// Claim-first modification creates one successor without overlapping its predecessor.
///
/// # Panics
/// Panics on wrong ownership, overlapping claims, duplicate successors or failed drain.
#[def_test]
fn modify_claimed_work_adds_one_owner_bound_successor() {
    let queue = queue(CpuPolicy::RoundRobin);
    let work = pin!(DelayedWork::new(|| {}));
    // SAFETY: pinned storage spans both controlled executions and physical timer drain.
    unsafe {
        work.as_ref()
            .modify_after(&queue, None, TimeSpan::ZERO, || panic!("RR"))
    }
    .unwrap();
    let partition = queue.partition(0.into()).unwrap();
    let running = partition.claim_next().unwrap();
    // SAFETY: running predecessor and new successor share retained external storage.
    unsafe {
        work.as_ref()
            .modify_after(&queue, None, later(), || panic!("owner"))
    }
    .unwrap();
    // SAFETY: zero modifies only the successor; the predecessor remains retained.
    unsafe {
        work.as_ref()
            .modify_after(&queue, None, TimeSpan::ZERO, || panic!("owner"))
    }
    .unwrap();
    assert!(partition.claim_next().is_none());
    running.run();
    partition.claim_next().unwrap().run();
    assert!(partition.claim_next().is_none());
    assert!(!work.cancel_sync().unwrap());
    assert_eq!(queue.select_cpu(None, || panic!("RR")), Ok(2.into()));
    assert_eq!(Arc::strong_count(&queue), 1);
}

/// Competing acceptance during route sampling is modified rather than reported duplicate.
///
/// # Panics
/// Panics on stale owner use, repeated sampling, duplicate admission or leaked Queue.
#[def_test]
fn modify_retries_competing_admission() {
    for delay in [TimeSpan::ZERO, later()] {
        let queue = queue(CpuPolicy::CurrentCpu);
        let work = pin!(DelayedWork::new(|| {}));
        let samples = Cell::new(0);
        let sample = || {
            samples.set(samples.get() + 1);
            // SAFETY: injected producer shares pinned storage drained after the outer call.
            unsafe { work.as_ref().queue_after_on(&queue, 0.into(), later()) }.unwrap();
            2.into()
        };
        assert_eq!(
            // SAFETY: outer and injected operations retain the same prepared pair through drain.
            unsafe { work.as_ref().modify_after(&queue, None, delay, sample) },
            Ok(0.into())
        );
        assert_eq!(samples.get(), 1);
        assert_eq!(
            work.work_for_test().state.lock().pending_is_ready(),
            delay.is_zero()
        );
        assert!(work.cancel_sync().unwrap());
        assert_eq!(Arc::strong_count(&queue), 1);
    }
}

/// Failed modification preserves timer state; disable and closure block fresh replacement.
///
/// # Panics
/// Panics on changed deadline, lost scope, incorrect rejection or unbalanced resources.
#[def_test]
fn modify_rejections_preserve_arrangement() {
    let queue = queue(CpuPolicy::RoundRobin);
    let other = self::queue(CpuPolicy::RoundRobin);
    let work = pin!(DelayedWork::new(|| {}));
    // SAFETY: prepared storage stays pinned through cancellation below.
    unsafe {
        work.as_ref()
            .modify_after(&queue, None, later(), || panic!("RR"))
    }
    .unwrap();
    let before = work.work_for_test().state.lock().delayed_deadline_ns();
    for delay in [TimeSpan::MAX, TimeSpan::from_nanos(u64::MAX)] {
        assert_eq!(
            // SAFETY: pinned Work/timer and captures stay retained through final drain.
            unsafe {
                work.as_ref()
                    .modify_after(&queue, None, delay, || panic!("owner"))
            },
            Err(ArmError::Timer(kerrno::KError::InvalidInput))
        );
    }
    for target_queue in [&queue, &other] {
        assert_eq!(
            // SAFETY: retained pinned storage and initialized timer service outlive this call.
            unsafe {
                work.as_ref()
                    .modify_after(target_queue, Some(2.into()), TimeSpan::ZERO, || {
                        panic!("explicit")
                    })
            },
            Err(ArmError::Admission(SubmitError::OwnerConflict))
        );
    }
    assert_eq!(
        work.work_for_test().state.lock().delayed_deadline_ns(),
        before
    );
    queue.close_admission();
    assert_eq!(
        // SAFETY: prepared storage remains pinned through the final synchronized drain.
        unsafe {
            work.as_ref()
                .modify_after(&queue, None, TimeSpan::ZERO, || panic!("closed"))
        },
        Err(ArmError::Admission(SubmitError::Closed))
    );
    assert_eq!(
        work.work_for_test().state.lock().delayed_deadline_ns(),
        before
    );
    assert!(work.cancel_sync().unwrap());
    let (guard, _) = work.work_for_test().disable();
    for delay in [TimeSpan::ZERO, later()] {
        assert_eq!(
            // SAFETY: disabled pinned storage remains valid until the guard and drain end.
            unsafe {
                work.as_ref()
                    .modify_after(&other, None, delay, || panic!("RR"))
            },
            Err(ArmError::Admission(SubmitError::Disabled))
        );
    }
    drop(guard);
    assert!(!work.cancel_sync().unwrap());
    assert_eq!(Arc::strong_count(&queue), 1);
    assert_eq!(Arc::strong_count(&other), 1);
}

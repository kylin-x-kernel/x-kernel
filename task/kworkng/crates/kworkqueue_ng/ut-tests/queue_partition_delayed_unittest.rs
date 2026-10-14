// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::{
    num::NonZeroUsize,
    pin::pin,
    sync::atomic::{AtomicUsize, Ordering},
};

use unittest::def_test;

use super::*;
use crate::{queue::queue_unittest::queue_for, work::observers::CompletionObserver};

/// Current deadline and color, not old delivery metadata, control the one transfer.
///
/// # Panics
/// Panics on early/duplicate execution, lost observers or wrong color accounting.
#[def_test]
fn due_transfer_rechecks_deadline_and_joins_current_batch_once() {
    let queue = queue_for(&[0], 1);
    let partition = queue.partition(0.into()).unwrap();
    let calls = AtomicUsize::new(0);
    let work = pin!(Work::with_callback(|| {
        calls.fetch_add(1, Ordering::Relaxed);
    }));
    partition.install_delayed_for_test(&queue, &work, 10);
    let (work_done, token) = CompletionObserver::prepare();
    assert!(work.capture(&mut alloc::vec![token]).is_ok());
    // Timer-only work does not extend ordinary flush or consume its color.
    let old_color = queue.current_color();
    queue.flush().unwrap();
    assert_ne!(queue.current_color(), old_color);
    // SAFETY: pinned Work and its capture outlive every publication and execution below.
    assert_eq!(unsafe { partition.queue_due(&work, 9) }, Ok(false));
    {
        let _partition = partition.state.lock();
        work.state.lock().retime_pending(20);
    }
    // SAFETY: same retained storage, with a modeled clock at the old deadline.
    assert_eq!(unsafe { partition.queue_due(&work, 10) }, Ok(false));
    assert_eq!(partition.state.lock().active, 0);
    // SAFETY: modeled clock reaches the current deadline; storage remains pinned.
    assert_eq!(unsafe { partition.queue_due(&work, 20) }, Ok(true));
    // SAFETY: duplicate delivery must not republish this still-retained Work.
    assert_eq!(unsafe { partition.queue_due(&work, 21) }, Ok(false));
    let (batch_done, token) = CompletionObserver::prepare();
    assert!(
        partition
            .observe_batch(queue.current_color(), token)
            .is_ok()
    );
    let (_, token) = CompletionObserver::prepare();
    assert!(partition.observe_batch(old_color, token).is_err());
    assert!(!work_done.is_completed());
    assert!(!batch_done.is_completed());
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    partition.claim_next().unwrap().run();
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert!(work_done.is_completed());
    assert!(batch_done.is_completed());
    assert!(partition.claim_next().is_none());
    assert_eq!(Arc::strong_count(&queue), 1);
}

/// A canceled timer has no list to unlink; an old route cannot publish its replacement.
///
/// # Panics
/// Panics on resurrection, wrong-owner publication, leaked resources or allowance.
#[def_test]
fn timer_cancellation_and_stale_route_do_not_publish_work() {
    let old_queue = queue_for(&[0], 1);
    let new_queue = queue_for(&[0], 1);
    let old_partition = old_queue.partition(0.into()).unwrap();
    let new_partition = new_queue.partition(0.into()).unwrap();
    let work = pin!(Work::with_callback(|| panic!("canceled work ran")));
    old_partition.install_delayed_for_test(&old_queue, &work, 10);
    assert!(work.cancel_pending());
    // SAFETY: retained pinned Work is idle; a late delivery must do nothing.
    assert_eq!(unsafe { old_partition.queue_due(&work, 10) }, Ok(false));
    new_partition.install_delayed_for_test(&new_queue, &work, 20);
    // SAFETY: stable Work now belongs to another partition, with no old publication.
    assert_eq!(unsafe { old_partition.queue_due(&work, 20) }, Ok(false));
    // SAFETY: new owner publishes the retained node, canceled immediately below.
    assert_eq!(unsafe { new_partition.queue_due(&work, 20) }, Ok(true));
    assert!(work.cancel_pending());
    assert_eq!(old_partition.state.lock().active, 0);
    assert_eq!(new_partition.state.lock().active, 0);
    assert_eq!(Arc::strong_count(&old_queue), 1);
    assert_eq!(Arc::strong_count(&new_queue), 1);
}

/// A due successor remains waiting while the same Work's predecessor is running.
///
/// # Panics
/// Panics on overlapping execution, premature allowance or incomplete drain.
#[def_test]
fn due_successor_waits_for_predecessor_retirement() {
    let queue = queue_for(&[0], 2);
    let partition = queue.partition(0.into()).unwrap();
    let calls = AtomicUsize::new(0);
    let work = pin!(Work::with_callback(|| {
        calls.fetch_add(1, Ordering::Relaxed);
    }));
    // SAFETY: pinned Work and captures outlive both occurrences and executions.
    unsafe { queue.submit(&work, None, || 0.into()) }.unwrap();
    let running = partition.claim_next().unwrap();
    partition.install_delayed_for_test(&queue, &work, 10);
    // SAFETY: successor uses the same retained external storage through both runs.
    assert_eq!(unsafe { partition.queue_due(&work, 10) }, Ok(true));
    assert!(partition.claim_next().is_none());
    assert_eq!(partition.state.lock().active, 1);
    running.run();
    partition.claim_next().unwrap().run();
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    assert_eq!(partition.state.lock().active, 0);
    queue.flush().unwrap();
}

/// Transfer joins the real FIFO waiting list without bypassing max_active.
///
/// # Panics
/// Panics if timer work skips earlier work, loses its node or leaks allowance.
#[def_test]
fn due_transfer_preserves_waiting_order_and_active_limit() {
    let queue = queue_for(&[0], 1);
    let partition = queue.partition(0.into()).unwrap();
    let trace = AtomicUsize::new(0);
    let first = pin!(Work::with_callback(|| {
        trace.store(1, Ordering::Relaxed);
    }));
    let timer = pin!(Work::with_callback(|| {
        assert_eq!(trace.swap(2, Ordering::Relaxed), 1);
    }));
    let later = pin!(Work::with_callback(|| {
        assert_eq!(trace.swap(3, Ordering::Relaxed), 2);
    }));
    // SAFETY: all three pinned objects remain live until the three runs below.
    unsafe { queue.submit(&first, None, || 0.into()) }.unwrap();
    partition.install_delayed_for_test(&queue, &timer, 10);
    // SAFETY: timer Work remains pinned until its eventual execution.
    assert_eq!(unsafe { partition.queue_due(&timer, 10) }, Ok(true));
    // SAFETY: later Work remains pinned until its eventual execution.
    unsafe { queue.submit(&later, None, || 0.into()) }.unwrap();
    for _ in 0..3 {
        assert_eq!(partition.state.lock().active, 1);
        partition.claim_next().unwrap().run();
    }
    assert_eq!(trace.load(Ordering::Relaxed), 3);
    assert_eq!(partition.state.lock().active, 0);
    queue.flush().unwrap();
}

/// A full count or closed queue leaves the timer-side occurrence unchanged.
///
/// # Panics
/// Panics on partial transfer, lost timer placement or erroneous count retirement.
#[def_test]
fn rejected_due_transfer_preserves_timer_placement() {
    let mut queue = queue_for(&[0], 1);
    Arc::get_mut(&mut queue).unwrap().partitions[0] = Arc::new(QueuePartition {
        in_flight: super::super::inflight::QueueInFlight::exhausted_for_test(),
        ..QueuePartition::new(0.into(), NonZeroUsize::new(1).unwrap())
    });
    queue.publish_color(1);
    let partition = queue.partition(0.into()).unwrap();
    let work = pin!(Work::with_callback(|| {}));
    partition.install_delayed_for_test(&queue, &work, 10);
    assert_eq!(
        // SAFETY: pinned storage is retained until logical cancellation; no physical timer.
        unsafe { partition.queue_due(&work, 10) },
        Err(SubmitError::CountOverflow)
    );
    assert_eq!(work.state.lock().delayed_deadline_ns(), Some(10));
    assert_eq!(partition.state.lock().active, 0);
    assert!(partition.state.lock().waiting.is_empty());
    queue.close_admission();
    assert_eq!(
        // SAFETY: closure rejects without publishing this retained pinned Work.
        unsafe { partition.queue_due(&work, 10) },
        Err(SubmitError::Closed)
    );
    assert!(work.cancel_pending());
    assert_eq!(Arc::strong_count(&queue), 1);
}

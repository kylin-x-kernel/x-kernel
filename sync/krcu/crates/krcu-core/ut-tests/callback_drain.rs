// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq};

use super::*;

fn enqueue_marker(queue: &CallbackQueue, marker: UnsafeRef<CallbackNode>) -> bool {
    let node = UnsafeRef::into_raw(marker);
    // SAFETY: unpublished owning fixture; its nonblocking callback only records
    // completion and needs no GP. The test retains and drains the queue.
    let accepted = unsafe { queue.enqueue_drain_marker(node) };
    if !accepted {
        // SAFETY: rejection retains the original unique unqueued fixture.
        let marker = unsafe { CallbackNode::from_raw(node) };
        // SAFETY: this unpublished marker's callback can run immediately to
        // release storage, even on test failure; it has no independent GP duty.
        unsafe { CallbackNode::invoke(marker) };
    }
    accepted
}

fn ordered_node(order: &Arc<AtomicUsize>, id: usize) -> UnsafeRef<CallbackNode> {
    let order = order.clone();
    Fixture::prepare(move || {
        let _ = order.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |old| {
            Some(old * 10 + id)
        });
    })
}

fn submit_ordered(queue: &CallbackQueue, order: &Arc<AtomicUsize>, id: usize) {
    let node = ordered_node(order, id);
    // SAFETY: uniquely owned unpublished fixture with nonblocking cleanup;
    // the test retains its queue and supplies explicit modeled GP completion.
    unsafe { queue.submit(node) };
}

fn execute(queue: &CallbackQueue, budget: usize) -> usize {
    // SAFETY: isolated fixtures publish no protected data. The test explicitly
    // drives eligibility and retains storage, with no bookkeeping locks held.
    unsafe { queue.invoke_ready(budget) }
}

#[unittest::def_test]
fn empty_queue_needs_no_marker_or_grace_period() {
    let nodes = [ReaderNode::new()];
    let gp = GracePeriodCoordinator::new(&nodes, [true]).unwrap();
    let queues = [CallbackQueue::new()];
    // SAFETY: exclusively controlled fresh coordinator and empty queues.
    let mut driver = unsafe { CallbackDriver::new(&gp, &queues) };
    // SAFETY: no outstanding callback; the absent marker must not be accessed.
    assert!(!unsafe { queues[0].enqueue_drain_marker(core::ptr::null_mut()) });
    assert_eq!(driver.drive(), None);
    assert_eq!(gp.completed_period(), 0);
    assert_eq!(queues[0].outstanding(), 0);
}

#[unittest::def_test]
fn marker_inherits_pending_waiting_or_eligible_tail_without_another_period() {
    for stage in 0..3 {
        let nodes = [ReaderNode::new()];
        let gp = GracePeriodCoordinator::new(&nodes, [true]).unwrap();
        let queues = [CallbackQueue::new()];
        // SAFETY: fresh isolated state, exclusively driven and drained below.
        let mut driver = unsafe { CallbackDriver::new(&gp, &queues) };
        let order = Arc::new(AtomicUsize::new(0));
        submit_ordered(&queues[0], &order, 1);
        if stage >= 1 {
            assert_eq!(driver.drive(), Some(1));
        }
        if stage == 2 {
            assert!(observe(&gp, 1));
            assert_eq!(driver.drive(), None);
        }
        assert!(enqueue_marker(&queues[0], ordered_node(&order, 2)));
        assert_eq!(queues[0].outstanding(), 2);
        if stage < 2 {
            assert_eq!(execute(&queues[0], 8), 0);
            assert_eq!(driver.drive(), Some(1));
            assert!(observe(&gp, 1));
        }
        assert_eq!(driver.drive(), None);
        assert_eq!(execute(&queues[0], 8), 2);
        assert_eq!(order.load(Ordering::Relaxed), 12);
        assert_eq!(queues[0].outstanding(), 0);
        assert_eq!(gp.completed_period(), 1);
    }
}

#[unittest::def_test]
fn marker_follows_unassigned_work_after_an_already_assigned_prefix() {
    let nodes = [ReaderNode::new()];
    let gp = GracePeriodCoordinator::new(&nodes, [true]).unwrap();
    let queues = [CallbackQueue::new()];
    // SAFETY: isolated controller and unpublished fixtures, drained below.
    let mut driver = unsafe { CallbackDriver::new(&gp, &queues) };
    let order = Arc::new(AtomicUsize::new(0));
    submit_ordered(&queues[0], &order, 1);
    assert_eq!(driver.drive(), Some(1));
    submit_ordered(&queues[0], &order, 2);
    assert!(enqueue_marker(&queues[0], ordered_node(&order, 3)));
    assert_eq!(driver.drive(), Some(1));
    assert!(observe(&gp, 1));
    assert_eq!(driver.drive(), Some(2));
    assert_eq!(execute(&queues[0], 8), 1);
    assert_eq!(order.load(Ordering::Relaxed), 1);
    assert!(observe(&gp, 2));
    assert_eq!(driver.drive(), None);
    assert_eq!(execute(&queues[0], 8), 2);
    assert_eq!(order.load(Ordering::Relaxed), 123);
    assert_eq!(queues[0].outstanding(), 0);
}

#[unittest::def_test]
fn marker_follows_newest_waiting_segment_after_slot_reuse_and_wrap() {
    for older in [1, PeriodId::MAX] {
        let queue = CallbackQueue::new();
        let order = Arc::new(AtomicUsize::new(0));
        // Model promotion explicitly to make the newest segment occupy slot
        // zero while the older target occupies slot one, including MAX -> 0.
        submit_ordered(&queue, &order, 1);
        assert!(queue.assign_pending(older.wrapping_sub(1)));
        submit_ordered(&queue, &order, 2);
        assert!(queue.assign_pending(older));
        queue.promote(older.wrapping_sub(1));
        assert_eq!(execute(&queue, 8), 1);
        submit_ordered(&queue, &order, 3);
        assert!(queue.assign_pending(older.wrapping_add(1)));
        assert!(enqueue_marker(&queue, ordered_node(&order, 4)));
        queue.promote(older);
        assert_eq!(execute(&queue, 8), 1);
        assert_eq!(order.load(Ordering::Relaxed), 12);
        queue.promote(older.wrapping_add(1));
        assert_eq!(execute(&queue, 8), 2);
        assert_eq!(order.load(Ordering::Relaxed), 1234);
        assert_eq!(queue.outstanding(), 0);
    }
}

#[unittest::def_test]
fn detached_remainder_stays_ahead_of_marker_across_budget_returns() {
    let nodes = [ReaderNode::new()];
    let gp = GracePeriodCoordinator::new(&nodes, [true]).unwrap();
    let queues = [CallbackQueue::new()];
    // SAFETY: exclusively controlled fixture, fully drained before destruction.
    let mut driver = unsafe { CallbackDriver::new(&gp, &queues) };
    let order = Arc::new(AtomicUsize::new(0));
    submit_ordered(&queues[0], &order, 1);
    submit_ordered(&queues[0], &order, 2);
    assert_eq!(driver.drive(), Some(1));
    assert!(observe(&gp, 1));
    assert_eq!(driver.drive(), None);
    let batch = CallbackBatch::take(&queues[0]).unwrap();
    assert!(!queues[0].has_ready());
    assert!(enqueue_marker(&queues[0], ordered_node(&order, 3)));
    assert_eq!(execute(&queues[0], 8), 0);
    drop(batch);
    assert_eq!(execute(&queues[0], 1), 1);
    assert_eq!(order.load(Ordering::Relaxed), 1);
    assert_eq!(execute(&queues[0], 1), 1);
    assert_eq!(order.load(Ordering::Relaxed), 12);
    assert_eq!(execute(&queues[0], 1), 1);
    assert_eq!(order.load(Ordering::Relaxed), 123);
    assert_eq!(queues[0].outstanding(), 0);
}

#[unittest::def_test]
fn marker_cannot_run_until_current_callback_returns() {
    let nodes = [ReaderNode::new()];
    let gp = GracePeriodCoordinator::new(&nodes, [true]).unwrap();
    let queues = Arc::new([CallbackQueue::new()]);
    // SAFETY: one isolated driver, with all accepted callbacks drained below.
    let mut driver = unsafe { CallbackDriver::new(&gp, &queues) };
    let phase = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(AtomicUsize::new(0));
    let marker_phase = phase.clone();
    let marker = Fixture::prepare(move || {
        let previous = marker_phase.load(Ordering::Relaxed);
        marker_phase.store(if previous == 1 { 2 } else { 99 }, Ordering::Relaxed);
    });
    let callback_queues = queues.clone();
    let callback_phase = phase.clone();
    let callback_observed = observed.clone();
    let callback = Fixture::prepare(move || {
        let accepted = enqueue_marker(&callback_queues[0], marker);
        let invoked = execute(&callback_queues[0], 8);
        callback_observed.store(usize::from(accepted) + 10 * invoked, Ordering::Relaxed);
        callback_phase.store(1, Ordering::Relaxed);
    });
    // SAFETY: unpublished nonblocking fixture; recursive execution never waits.
    unsafe { queues[0].submit(callback) };
    assert_eq!(driver.drive(), Some(1));
    assert!(observe(&gp, 1));
    assert_eq!(driver.drive(), None);
    assert_eq!(execute(&queues[0], 8), 1);
    assert_eq!(observed.load(Ordering::Relaxed), 1);
    assert_eq!(phase.load(Ordering::Relaxed), 1);
    assert_eq!(queues[0].outstanding(), 1);
    assert_eq!(driver.drive(), None);
    assert_eq!(execute(&queues[0], 8), 1);
    assert_eq!(phase.load(Ordering::Relaxed), 2);
    assert_eq!(queues[0].outstanding(), 0);
}

#[unittest::def_test]
fn marker_does_not_follow_a_later_callback_repost() {
    let nodes = [ReaderNode::new()];
    let gp = GracePeriodCoordinator::new(&nodes, [true]).unwrap();
    let queues = Arc::new([CallbackQueue::new()]);
    // SAFETY: fresh isolated state; both original work and the repost are drained.
    let mut driver = unsafe { CallbackDriver::new(&gp, &queues) };
    let order = Arc::new(AtomicUsize::new(0));
    let child = ordered_node(&order, 3);
    let callback_order = order.clone();
    let callback_queues = queues.clone();
    let parent = Fixture::prepare(move || {
        callback_order.store(1, Ordering::Relaxed);
        // SAFETY: preallocated unpublished child; ownership transfers once and
        // the test retains the queue through its independently required GP.
        unsafe { callback_queues[0].submit(child) };
    });
    // SAFETY: owning nonblocking fixture with no real protected publication.
    unsafe { queues[0].submit(parent) };
    assert_eq!(driver.drive(), Some(1));
    assert!(enqueue_marker(&queues[0], ordered_node(&order, 2)));
    assert!(observe(&gp, 1));
    assert_eq!(driver.drive(), None);
    assert_eq!(execute(&queues[0], 1), 1);
    assert_eq!(driver.drive(), Some(2));
    // The marker precedes the repost and does not wait for the repost's GP.
    assert_eq!(execute(&queues[0], 8), 1);
    assert_eq!(order.load(Ordering::Relaxed), 12);
    assert_eq!(queues[0].outstanding(), 1);
    assert!(observe(&gp, 2));
    assert_eq!(driver.drive(), None);
    assert_eq!(execute(&queues[0], 8), 1);
    assert_eq!(order.load(Ordering::Relaxed), 123);
    assert_eq!(queues[0].outstanding(), 0);
}

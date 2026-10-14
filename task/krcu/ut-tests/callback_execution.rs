// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#[cfg(not(test))]
use unittest::{assert, assert_eq};

use super::*;

fn execute(queue: &CallbackQueue, budget: usize) -> usize {
    // SAFETY: deterministic fixtures retire no published data and hold no RCU
    // locks. Their owned callbacks use nonblocking operations and real queue/GP
    // transitions, with explicitly supplied simulated CPU observations.
    unsafe { queue.invoke_ready(budget) }
}

fn submit_ordered(queue: &CallbackQueue, order: &Arc<AtomicUsize>, id: usize) {
    let order = order.clone();
    let node = Fixture::prepare(move || {
        let _ = order.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |old| {
            Some(old * 10 + id)
        });
    });
    // SAFETY: unpublished, owned nonblocking fixture; test retains and drains
    // the queue before releasing fixture storage.
    unsafe { queue.submit(node) };
}

#[cfg_attr(test, test)]
#[cfg_attr(not(test), unittest::def_test)]
fn budget_limits_execution_without_replaying_completed_callbacks() {
    let nodes = [ReaderNode::new()];
    let gp = GracePeriodCoordinator::new(&nodes, [true]).unwrap();
    let queues = [CallbackQueue::new()];
    // SAFETY: fresh isolated coordinator/queues; this fixture is their sole
    // controller and drains all work before releasing the storage.
    let mut driver = unsafe { CallbackDriver::new(&gp, &queues) };
    let order = Arc::new(AtomicUsize::new(0));
    for id in 1..=4 {
        submit_ordered(&queues[0], &order, id);
    }
    assert_eq!(execute(&queues[0], 4), 0);
    assert_eq!(driver.drive(), Some(1));
    assert!(observe(&gp, 1));
    assert_eq!(driver.drive(), None);
    assert_eq!(execute(&queues[0], 0), 0);
    assert_eq!(queues[0].outstanding(), 4);
    assert_eq!(execute(&queues[0], 1), 1);
    assert_eq!(order.load(Ordering::Relaxed), 1);
    assert_eq!(queues[0].outstanding(), 3);
    assert!(queues[0].has_ready());
    assert_eq!(execute(&queues[0], 2), 2);
    assert_eq!(order.load(Ordering::Relaxed), 123);
    assert_eq!(execute(&queues[0], 8), 1);
    assert_eq!(order.load(Ordering::Relaxed), 1234);
    assert_eq!(queues[0].outstanding(), 0);
    assert!(!queues[0].has_ready());
    assert_eq!(execute(&queues[0], 1), 0);
}

#[cfg_attr(test, test)]
#[cfg_attr(not(test), unittest::def_test)]
fn detached_batch_stays_outstanding_and_precedes_newly_eligible_work() {
    let nodes = [ReaderNode::new()];
    let gp = GracePeriodCoordinator::new(&nodes, [true]).unwrap();
    let queues = [CallbackQueue::new()];
    // SAFETY: fresh isolated coordinator/queues; this fixture is their sole
    // controller and drains all work before releasing the storage.
    let mut driver = unsafe { CallbackDriver::new(&gp, &queues) };
    let order = Arc::new(AtomicUsize::new(0));
    submit_ordered(&queues[0], &order, 1);
    submit_ordered(&queues[0], &order, 2);
    assert_eq!(driver.drive(), Some(1));
    assert!(observe(&gp, 1));
    assert_eq!(driver.drive(), None);
    let batch = CallbackBatch::take(&queues[0]).unwrap();
    assert!(!queues[0].has_ready());
    assert_eq!(queues[0].outstanding(), 2);
    assert_eq!(execute(&queues[0], 8), 0);
    // Pause the owning executor while the real period driver makes more work
    // eligible. GP progress does not wait for callback execution to finish.
    submit_ordered(&queues[0], &order, 3);
    assert_eq!(driver.drive(), Some(2));
    assert!(observe(&gp, 2));
    assert_eq!(driver.drive(), None);
    assert!(queues[0].has_ready());
    assert_eq!(queues[0].outstanding(), 3);
    assert_eq!(execute(&queues[0], 8), 0);
    drop(batch);
    assert_eq!(execute(&queues[0], 1), 1);
    assert_eq!(execute(&queues[0], 8), 2);
    assert_eq!(order.load(Ordering::Relaxed), 123);
    assert_eq!(queues[0].outstanding(), 0);
}

#[cfg_attr(test, test)]
#[cfg_attr(not(test), unittest::def_test)]
fn running_callback_remains_outstanding_and_recursive_executor_cannot_overtake() {
    let nodes = [ReaderNode::new()];
    let gp = GracePeriodCoordinator::new(&nodes, [true]).unwrap();
    let queues = Arc::new([CallbackQueue::new()]);
    // SAFETY: fresh isolated coordinator/queues; this fixture is their sole
    // controller and drains all work before releasing the storage.
    let mut driver = unsafe { CallbackDriver::new(&gp, &queues) };
    let observed = Arc::new(AtomicUsize::new(0));
    let callback_queues = queues.clone();
    let callback_observed = observed.clone();
    let node = Fixture::prepare(move || {
        // These acquisitions also verify invocation is outside the queue lock.
        let outstanding = callback_queues[0].outstanding();
        let competitor = execute(&callback_queues[0], 8);
        callback_observed.store(outstanding * 10 + competitor, Ordering::Relaxed);
    });
    // SAFETY: owned nonblocking fixture; the callback retains queue storage and
    // only attempts a nonwaiting recursive execution, never waits for progress.
    unsafe { queues[0].submit(node) };
    let other = Arc::new(AtomicUsize::new(0));
    submit(&queues[0], &other);
    assert_eq!(driver.drive(), Some(1));
    assert!(observe(&gp, 1));
    assert_eq!(driver.drive(), None);
    assert_eq!(execute(&queues[0], 1), 1);
    assert_eq!(observed.load(Ordering::Relaxed), 20);
    assert_eq!(other.load(Ordering::Relaxed), 0);
    assert_eq!(queues[0].outstanding(), 1);
    assert_eq!(execute(&queues[0], 1), 1);
    assert_eq!(other.load(Ordering::Relaxed), 1);
    assert_eq!(queues[0].outstanding(), 0);
}

#[cfg_attr(test, test)]
#[cfg_attr(not(test), unittest::def_test)]
fn finite_repost_requires_a_new_period_and_is_not_chased_by_current_batch() {
    let nodes = [ReaderNode::new()];
    let gp = GracePeriodCoordinator::new(&nodes, [true]).unwrap();
    let queues = Arc::new([CallbackQueue::new()]);
    // SAFETY: fresh isolated coordinator/queues; this fixture is their sole
    // controller and drains all work before releasing the storage.
    let mut driver = unsafe { CallbackDriver::new(&gp, &queues) };
    let calls = Arc::new(AtomicUsize::new(0));
    let child_calls = calls.clone();
    let child = Fixture::prepare(move || {
        child_calls.fetch_add(1, Ordering::Relaxed);
    });
    let parent_calls = calls.clone();
    let callback_queues = queues.clone();
    let parent = Fixture::prepare(move || {
        parent_calls.fetch_add(1, Ordering::Relaxed);
        // SAFETY: child was independently prepared before this callback; it
        // owns an atomic counter, and the test retains/drains this same queue.
        unsafe { callback_queues[0].submit(child) };
    });
    // SAFETY: uniquely owned nonblocking fixture; no protected data is retired.
    unsafe { queues[0].submit(parent) };
    assert_eq!(driver.drive(), Some(1));
    assert!(observe(&gp, 1));
    assert_eq!(driver.drive(), None);
    assert_eq!(execute(&queues[0], 8), 1);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(queues[0].outstanding(), 1);
    assert_eq!(execute(&queues[0], 8), 0);
    assert_eq!(driver.drive(), Some(2));
    assert!(observe(&gp, 2));
    assert_eq!(driver.drive(), None);
    assert_eq!(execute(&queues[0], 8), 1);
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    assert_eq!(queues[0].outstanding(), 0);
}

#[cfg_attr(test, test)]
#[cfg_attr(not(test), unittest::def_test)]
fn prepared_burst_has_no_fixed_queue_capacity() {
    let nodes = [ReaderNode::new()];
    let gp = GracePeriodCoordinator::new(&nodes, [true]).unwrap();
    let queues = [CallbackQueue::new()];
    // SAFETY: fresh isolated coordinator/queues; this fixture is their sole
    // controller and drains all work before releasing the storage.
    let mut driver = unsafe { CallbackDriver::new(&gp, &queues) };
    let calls = Arc::new(AtomicUsize::new(0));
    let count = 1024;
    for _ in 0..count {
        submit(&queues[0], &calls);
    }
    assert_eq!(queues[0].outstanding(), count);
    assert_eq!(driver.drive(), Some(1));
    assert!(observe(&gp, 1));
    assert_eq!(driver.drive(), None);
    for batch in 0..count.div_ceil(7) {
        assert_eq!(execute(&queues[0], 7), (count - batch * 7).min(7));
    }
    assert!(!queues[0].has_ready());
    assert_eq!(calls.load(Ordering::Relaxed), count);
    assert_eq!(queues[0].outstanding(), 0);
}

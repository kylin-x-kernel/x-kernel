// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, sync::Arc};
use core::sync::atomic::AtomicUsize;

use intrusive_collections::container_of;
use unittest::{assert, assert_eq};

use super::*;
use crate::{ReaderNode, TaskReader};

#[repr(C)]
struct Fixture<F> {
    node: CallbackNode,
    callback_fn: F,
}

impl<F: FnOnce() + Send + 'static> Fixture<F> {
    fn prepare(callback_fn: F) -> UnsafeRef<CallbackNode> {
        let object = Box::into_raw(Box::new(Self {
            node: CallbackNode::new(Self::invoke),
            callback_fn,
        }));
        // SAFETY: initialized Box; raw projection retains enclosing provenance.
        let node = unsafe { &raw mut (*object).node };
        // SAFETY: ownership transfers to the fixture queue, with a Send payload
        // and exactly one terminal invocation that reconstructs this Box.
        unsafe { CallbackNode::from_raw(node) }
    }

    /// # Safety
    /// `node` is the unique detached node from this concrete Fixture::prepare.
    unsafe fn invoke(node: *mut CallbackNode) {
        // SAFETY: prepare used raw projection from this concrete allocation.
        let object = unsafe { container_of!(node, Self, node) }.cast_mut();
        // SAFETY: unique terminal invocation; no other owner or borrow remains.
        let object = unsafe { Box::from_raw(object) };
        let Self { callback_fn, .. } = *object;
        callback_fn();
    }
}

fn submit(queue: &CallbackQueue, calls: &Arc<AtomicUsize>) {
    let calls = calls.clone();
    let node = Fixture::prepare(move || {
        calls.fetch_add(1, Ordering::Relaxed);
    });
    // SAFETY: isolated unpublished fixture, nonblocking owned atomic payload;
    // tests retain/drain the queue and explicitly supply simulated observations.
    unsafe { queue.submit(node) };
}

fn execute_all(queue: &CallbackQueue) {
    // SAFETY: fixtures publish no protected data; observations drive the real
    // coordinator and callbacks are nonblocking, with no RCU locks held.
    unsafe { queue.invoke_ready(usize::MAX) };
}

#[path = "callback_execution.rs"]
mod execution;

fn observe<const CPUS: usize>(gp: &GracePeriodCoordinator<'_, CPUS>, period: PeriodId) -> bool {
    let mut observed = true;
    for cpu in 0..CPUS {
        observed &= gp.observe_cpu(cpu, period);
        let _ = gp.report_node(cpu);
    }
    observed
}

fn eligible_count(queue: &CallbackQueue) -> usize {
    queue.lock().eligible.iter().count()
}

#[unittest::def_test]
fn moving_driver_preserves_active_period_and_successor_work() {
    let nodes = [ReaderNode::new()];
    let gp = GracePeriodCoordinator::new(&nodes, [true]).unwrap();
    let queues = [CallbackQueue::new()];
    // SAFETY: fresh isolated state, controlled only by this driver; the handoff
    // moves that owner rather than constructing a replacement over an active GP.
    let mut driver = unsafe { CallbackDriver::new(&gp, &queues) };
    let calls = Arc::new(AtomicUsize::new(0));
    submit(&queues[0], &calls);
    assert_eq!(driver.drive(), Some(1));
    submit(&queues[0], &calls);
    assert_eq!(driver.drive(), Some(1));
    let mut successor = driver;
    assert!(observe(&gp, 1));
    // No new prefix: the retained successor segment must still start a GP.
    assert_eq!(successor.drive(), Some(2));
    execute_all(&queues[0]);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert!(observe(&gp, 2));
    assert_eq!(successor.drive(), None);
    execute_all(&queues[0]);
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    assert_eq!(queues[0].outstanding(), 0);
}

#[unittest::def_test]
fn callbacks_on_either_side_of_start_require_distinct_periods() {
    let nodes = [ReaderNode::new()];
    let gp = GracePeriodCoordinator::new(&nodes, [true]).unwrap();
    let queues = [CallbackQueue::new()];
    // SAFETY: fresh isolated coordinator/queues; this fixture is their sole
    // controller and drains all work before releasing the storage.
    let mut driver = unsafe { CallbackDriver::new(&gp, &queues) };
    let calls = Arc::new(AtomicUsize::new(0));
    submit(&queues[0], &calls);
    assert_eq!(driver.drive(), Some(1));
    submit(&queues[0], &calls);
    assert_eq!(driver.drive(), Some(1));
    assert_eq!(eligible_count(&queues[0]), 0);
    assert!(observe(&gp, 1));
    assert_eq!(driver.drive(), Some(2));
    assert_eq!(eligible_count(&queues[0]), 1);
    execute_all(&queues[0]);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert!(observe(&gp, 2));
    assert_eq!(driver.drive(), None);
    execute_all(&queues[0]);
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    assert_eq!(driver.drive(), None);
}

#[unittest::def_test]
fn submission_after_prefix_capture_does_not_join_that_prefix() {
    let nodes = [ReaderNode::new()];
    let gp = GracePeriodCoordinator::new(&nodes, [true]).unwrap();
    let queues = [CallbackQueue::new()];
    // SAFETY: fresh isolated coordinator/queues; this fixture is their sole
    // controller and drains all work before releasing the storage.
    let mut driver = unsafe { CallbackDriver::new(&gp, &queues) };
    let calls = Arc::new(AtomicUsize::new(0));
    submit(&queues[0], &calls);
    // Pause the actual capture/assignment transactions, without a production
    // observer hook or relying on wall-clock scheduling to hit the window.
    let prefix = queues[0].capture();
    submit(&queues[0], &calls);
    queues[0].assign(prefix, gp.completed_period().wrapping_add(1));
    driver.active_period = gp.begin_period();
    assert!(observe(&gp, 1));
    assert_eq!(driver.drive(), Some(2));
    assert_eq!(eligible_count(&queues[0]), 1);
    execute_all(&queues[0]);
    assert!(observe(&gp, 2));
    assert_eq!(driver.drive(), None);
    execute_all(&queues[0]);
    assert_eq!(calls.load(Ordering::Relaxed), 2);
}

#[unittest::def_test]
fn old_reader_blocks_execution_after_all_cpu_reports() {
    let nodes = [ReaderNode::new()];
    let gp = GracePeriodCoordinator::new(&nodes, [true]).unwrap();
    let queues = [CallbackQueue::new()];
    // SAFETY: fresh isolated coordinator/queues; this fixture is their sole
    // controller and drains all work before releasing the storage.
    let mut driver = unsafe { CallbackDriver::new(&gp, &queues) };
    let calls = Arc::new(AtomicUsize::new(0));
    let reader = TaskReader::new();
    let during = reader.with_read(|_| {
        let _guard = kspin::NoPreemptIrqSave::new();
        // SAFETY: stable isolated fixture; this scope removes enrollment before
        // the reader/node storage ends, with no competing scheduler.
        unsafe { nodes[0].enroll(&reader) };
        submit(&queues[0], &calls);
        let started = driver.drive();
        let observed = observe(&gp, 1);
        (
            started,
            observed,
            driver.drive(),
            eligible_count(&queues[0]),
        )
    });
    assert_eq!(during, (Some(1), true, Some(1), 0));
    assert!(gp.report_node(0));
    assert_eq!(driver.drive(), None);
    execute_all(&queues[0]);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

#[unittest::def_test]
fn eligibility_on_every_queue_survives_multiple_counter_wraps() {
    let nodes = [ReaderNode::new(), ReaderNode::new()];
    let gp = GracePeriodCoordinator::new(&nodes, [true, true]).unwrap();
    let queues = [CallbackQueue::new(), CallbackQueue::new()];
    // SAFETY: fresh isolated coordinator/queues; this fixture is their sole
    // controller and drains all work before releasing the storage.
    let mut driver = unsafe { CallbackDriver::new(&gp, &queues) };
    let delayed = Arc::new(AtomicUsize::new(0));
    let immediate = Arc::new(AtomicUsize::new(0));
    submit(&queues[0], &delayed);
    submit(&queues[1], &immediate);
    assert_eq!(driver.drive(), Some(1));
    assert!(observe(&gp, 1));
    assert_eq!(driver.drive(), None);
    assert_eq!(eligible_count(&queues[0]), 1);
    assert_eq!(eligible_count(&queues[1]), 1);
    execute_all(&queues[1]);
    let periods = 2 * (usize::from(PeriodId::MAX) + 1);
    let mut period: PeriodId = 1;
    for _ in 0..periods {
        submit(&queues[1], &immediate);
        period = period.wrapping_add(1);
        assert_eq!(driver.drive(), Some(period));
        assert!(observe(&gp, period));
        assert_eq!(driver.drive(), None);
        assert_eq!(eligible_count(&queues[0]), 1);
        execute_all(&queues[1]);
    }
    execute_all(&queues[0]);
    assert_eq!(delayed.load(Ordering::Relaxed), 1);
    assert_eq!(immediate.load(Ordering::Relaxed), periods + 1);
}

#[unittest::def_test]
fn repeated_captures_merge_the_successor_segment_in_submission_order() {
    let nodes = [ReaderNode::new()];
    let gp = GracePeriodCoordinator::new(&nodes, [true]).unwrap();
    let queues = [CallbackQueue::new()];
    // SAFETY: fresh isolated coordinator/queues; this fixture is their sole
    // controller and drains all work before releasing the storage.
    let mut driver = unsafe { CallbackDriver::new(&gp, &queues) };
    let order = Arc::new(AtomicUsize::new(0));
    for id in 1..=3 {
        let order = order.clone();
        let node = Fixture::prepare(move || {
            let _ = order.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |old| {
                Some(old * 10 + id)
            });
        });
        // SAFETY: unpublished fixture, owned nonblocking counter callback;
        // the queue stays alive through both periods and complete execution.
        unsafe { queues[0].submit(node) };
        assert_eq!(driver.drive(), Some(1));
    }
    assert!(observe(&gp, 1));
    assert_eq!(driver.drive(), Some(2));
    execute_all(&queues[0]);
    assert_eq!(order.load(Ordering::Relaxed), 1);
    assert!(observe(&gp, 2));
    assert_eq!(driver.drive(), None);
    execute_all(&queues[0]);
    assert_eq!(order.load(Ordering::Relaxed), 123);
}

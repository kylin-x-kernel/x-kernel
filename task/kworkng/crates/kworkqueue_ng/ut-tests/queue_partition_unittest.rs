// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, sync::Arc};
use core::{
    assert, assert_eq,
    num::NonZeroUsize,
    pin::pin,
    ptr::NonNull,
    sync::atomic::{AtomicUsize, Ordering},
};

use unittest::def_test;

// Kernel panic is terminal. Do not use returning assertion macros while a list
// borrows local storage: early return could release that storage before draining.
use super::{QueuePartition, inflight::QueueInFlight};
use crate::work::{Work, observers::CompletionObserver};

struct Resource(Option<Box<dyn FnOnce() + Send>>);

/// Checks closure after the final admission sample does not lose an old obligation.
///
/// # Panics
/// Panics if closing retroactively rejects sampled admission or its count is lost.
#[def_test]
fn closure_after_final_sample_preserves_accepted_count() {
    let queue = crate::queue::queue_unittest::queue_for(&[0], 1);
    let partition = Arc::new(QueuePartition::new(
        kcpu_id_map::LogicalCpuId::new(0),
        NonZeroUsize::new(1).unwrap(),
    ));
    let work = pin!(Work::with_callback(|| {}));
    // Test the shared transaction at the exact read/commit boundary. This isolated
    // partition tests accounting, not Queue's production partition selection.
    assert!(
        // SAFETY: pinned Work stays live through cancellation and there are no peers.
        unsafe {
            partition.accept_using(&work, queue.clone(), || {
                let color = queue.admission_color();
                queue.close_admission();
                color
            })
        }
        .is_ok()
    );
    let (receipt, token) = CompletionObserver::prepare();
    assert!(partition.observe_batch(0, token).is_ok());
    assert!(!receipt.is_completed());
    assert!(work.cancel_pending());
    assert!(receipt.is_completed());
    assert!(
        // SAFETY: same retained storage; a post-close sample rejects without effects.
        unsafe { partition.accept_using(&work, queue.clone(), || queue.admission_color()) }
            .is_err()
    );
    assert_eq!(Arc::strong_count(&queue), 1);
    assert_eq!(work.observer_count(), 0);
}

fn assert_pending<T>(partition: &QueuePartition, expected: &[&Work<T>]) {
    let mut index = 0;
    let mut state = partition.state.lock();
    let mut inspect = |node| {
        assert_eq!(node, NonNull::from(expected[index].pending_node()));
        index += 1;
        false
    };
    assert!(state.ready.pop_first_matching(&mut inspect).is_none());
    assert!(state.waiting.pop_first_matching(inspect).is_none());
    assert_eq!(index, expected.len());
}

/// Checks count exhaustion rejects admission before modifying Work storage.
///
/// # Panics
/// Panics if exhaustion changes Work, consumes resources or wraps the batch count.
#[def_test]
fn exhausted_batch_preserves_work_and_resources() {
    let partition = Arc::new(QueuePartition {
        in_flight: QueueInFlight::exhausted_for_test(),
        ..QueuePartition::new(
            kcpu_id_map::LogicalCpuId::new(0),
            NonZeroUsize::new(1).unwrap(),
        )
    });
    let work = pin!(Work::new());
    // SAFETY: pinned Work remains live through cancellation; no concurrent users.
    assert_eq!(unsafe { partition.accept(&work, 3, 1) }, Err(3));
    assert_eq!(work.observer_count(), 0);
    // SAFETY: same pinned storage, drained below before release.
    assert_eq!(unsafe { partition.accept(&work, 4, 1) }, Err(4));
    assert_pending::<usize>(&partition, &[]);
    // SAFETY: same pinned storage, drained below before release.
    assert!(unsafe { partition.accept(&work, 5, 0) }.is_ok());
    assert!(work.cancel_pending());
}

/// Checks Queue-entry count exhaustion is distinguishable and consumes no RR turn.
///
/// # Panics
/// Panics if exhaustion publishes Work, leaks Queue ownership or advances routing.
#[def_test]
fn queue_count_exhaustion_preserves_routing() {
    use crate::queue::{
        Queue,
        config::{CpuPolicy, QueueConfig},
        submission::SubmitError,
    };
    let cpu = kcpu_id_map::LogicalCpuId::new;
    let mut queue = Queue::new(
        QueueConfig::validate(&[cpu(0), cpu(2)], CpuPolicy::RoundRobin, 1, |_| true).unwrap(),
    );
    let partition = Arc::new(QueuePartition {
        in_flight: QueueInFlight::exhausted_for_test(),
        ..QueuePartition::new(cpu(0), NonZeroUsize::new(1).unwrap())
    });
    Arc::get_mut(&mut queue).unwrap().partitions[0] = partition.clone();
    queue.control.store(1, Ordering::SeqCst);
    let work = pin!(Work::with_callback(|| {}));
    assert_eq!(
        // SAFETY: count exhaustion rejects this retained pinned object before publication.
        unsafe { queue.submit(&work, None, || panic!("RR")) },
        Err(SubmitError::CountOverflow)
    );
    assert_eq!(work.observer_count(), 0);
    assert_eq!(Arc::strong_count(&queue), 1);
    assert_eq!(partition.state.lock().active, 0);
    assert_eq!(queue.select_cpu(None, || cpu(2)).unwrap(), cpu(0));
}

impl Drop for Resource {
    fn drop(&mut self) {
        if let Some(callback) = self.0.take() {
            callback();
        }
    }
}

/// Checks released-but-unclaimed work reserves allowance and cancellation refills it.
///
/// # Panics
/// Panics on double charging, waiting charges or lost refill after ready cancellation.
#[def_test]
fn released_work_reserves_allowance_before_claim() {
    let partition = Arc::new(QueuePartition::new(
        kcpu_id_map::LogicalCpuId::new(0),
        NonZeroUsize::new(4).unwrap(),
    ));
    let works = pin!([
        Work::new(),
        Work::new(),
        Work::new(),
        Work::new(),
        Work::new(),
        Work::new()
    ]);
    for work in works.iter() {
        // SAFETY: the pinned array remains live until every occurrence is drained below.
        assert!(unsafe { partition.accept(work, (), 0) }.is_ok());
    }
    assert_eq!(partition.state.lock().active, 4);
    for (index, work) in works.iter().enumerate() {
        assert_eq!(work.state.lock().pending_is_ready(), index < 4);
    }
    assert!(works[5].cancel_pending());
    assert_eq!(partition.state.lock().active, 4);
    assert!(!works[4].state.lock().pending_is_ready());
    let first = partition.claim_next().unwrap();
    assert_eq!(partition.state.lock().active, 4);
    assert!(works[1].cancel_pending());
    assert!(works[4].state.lock().pending_is_ready());
    assert_eq!(partition.state.lock().active, 4);
    first.run();
    while let Some(execution) = partition.claim_next() {
        execution.run();
    }
    assert_eq!(partition.state.lock().active, 0);
    let (_, token) = CompletionObserver::prepare();
    assert!(partition.observe_batch(0, token).is_err());
}

/// Checks cancellation refills ready allowance before detached resources are destroyed.
///
/// # Panics
/// Panics if cleanup holds allowance, or if its original batch completes too soon.
#[def_test]
fn ready_cancel_refills_before_cleanup_retires_batch() {
    let partition = Arc::new(QueuePartition::new(
        kcpu_id_map::LogicalCpuId::new(0),
        NonZeroUsize::new(1).unwrap(),
    ));
    let canceled = pin!(Work::new());
    let next = pin!(Work::new());
    let (receipt, token) = CompletionObserver::prepare();
    let cleanup_receipt = receipt.clone();
    let cleanup_partition = partition.clone();
    let resource = Resource(Some(Box::new(move || {
        assert!(!cleanup_receipt.is_completed());
        assert_eq!(cleanup_partition.state.lock().active, 1);
        cleanup_partition
            .claim_next()
            .expect("waiting work was released")
            .run();
        assert!(!cleanup_receipt.is_completed());
        assert_eq!(cleanup_partition.state.lock().active, 0);
    })));
    // SAFETY: both pinned objects outlive cancellation, including its reentrant execution.
    assert!(unsafe { partition.accept(&canceled, resource, 0) }.is_ok());
    // SAFETY: next remains pinned through execution inside cancellation's destructor.
    assert!(unsafe { partition.accept(&next, (), 1) }.is_ok());
    assert!(partition.observe_batch(0, token).is_ok());
    assert!(canceled.cancel_pending());
    assert!(receipt.is_completed());
    assert!(partition.claim_next().is_none());
}

/// Checks an unblocked early waiter cannot overtake already released work.
///
/// # Panics
/// Panics if release revokes earlier reservations to restore waiting order.
#[def_test]
fn already_released_work_keeps_precedence() {
    let partition = Arc::new(QueuePartition::new(
        kcpu_id_map::LogicalCpuId::new(0),
        NonZeroUsize::new(2).unwrap(),
    ));
    let order = AtomicUsize::new(0);
    let first = pin!(Work::with_callback(|| {
        order.fetch_add(1, Ordering::Relaxed);
    }));
    let second = pin!(Work::with_callback(|| {
        assert_eq!(order.load(Ordering::Relaxed), 1);
    }));
    // SAFETY: both pinned Work objects and their captures remain live through final drain.
    assert!(unsafe { partition.accept(&first, (), 0) }.is_ok());
    let old = partition.claim_next().unwrap();
    // SAFETY: successor uses the same retained pinned object.
    assert!(unsafe { partition.accept(&first, (), 1) }.is_ok());
    // SAFETY: second is drained before releasing its pinned storage.
    assert!(unsafe { partition.accept(&second, (), 0) }.is_ok());
    old.run();
    partition.claim_next().unwrap().run();
    partition.claim_next().unwrap().run();
    assert_eq!(order.load(Ordering::Relaxed), 2);
    assert_eq!(partition.state.lock().active, 0);
}

/// Checks running ownership and allowance persist through reentrant destruction.
///
/// # Panics
/// Panics if another partition accepts a successor or allowance is released early.
#[def_test]
fn cleanup_keeps_original_partition_and_allowance() {
    let first = Arc::new(QueuePartition::new(
        kcpu_id_map::LogicalCpuId::new(0),
        NonZeroUsize::new(1).unwrap(),
    ));
    let other = Arc::new(QueuePartition::new(
        kcpu_id_map::LogicalCpuId::new(0),
        NonZeroUsize::new(1).unwrap(),
    ));
    let work = Arc::new(Work::new());
    let second = Arc::new(Work::new());
    let weak_work = Arc::downgrade(&work);
    let cleanup_partition = first.clone();
    let cleanup_other = other.clone();
    let cleanup_second = second.clone();
    let resource = Resource(Some(Box::new(move || {
        let work = weak_work.upgrade().unwrap();
        // SAFETY: outer owner retains Work through successor cleanup.
        core::assert!(unsafe { cleanup_other.accept(&work, Resource(None), 0) }.is_err());
        core::assert!(cleanup_partition.claim(&cleanup_second).is_none());
        // SAFETY: outer owner retains Work through successor cleanup.
        core::assert!(unsafe { cleanup_partition.accept(&work, Resource(None), 1) }.is_ok());
        core::assert!(work.claim().is_none());
    })));
    // SAFETY: retained Arc storage is drained below before the owner is released.
    assert!(unsafe { first.accept(&work, resource, 0) }.is_ok());
    let execution = first.claim(&work).unwrap();
    // SAFETY: outer Arcs keep both Work allocations stable through final drain.
    assert!(unsafe { first.accept(&second, Resource(None), 0) }.is_ok());
    // SAFETY: Work's stable allocation remains owned until all occurrences finish.
    assert!(unsafe { other.accept(&work, Resource(None), 0) }.is_err());
    execution.run();
    assert_eq!(first.state.lock().active, 1);
    second.claim().unwrap().run();
    work.claim().unwrap().run();
    // SAFETY: final cancellation below drains this same retained allocation.
    assert!(unsafe { other.accept(&work, Resource(None), 0) }.is_ok());
    assert!(work.cancel_pending());
}

/// Checks stale partition lookup cannot claim or charge a replacement occurrence.
///
/// # Panics
/// Panics if an old route claims replacement work or alters its batch count.
#[def_test]
fn stale_partition_cannot_claim_replacement() {
    let first = Arc::new(QueuePartition::new(
        kcpu_id_map::LogicalCpuId::new(0),
        NonZeroUsize::new(1).unwrap(),
    ));
    let next = Arc::new(QueuePartition::new(
        kcpu_id_map::LogicalCpuId::new(0),
        NonZeroUsize::new(1).unwrap(),
    ));
    let work = pin!(Work::new());
    // SAFETY: pinned storage remains alive through both synchronous retirements.
    assert!(unsafe { first.accept(&work, 1, 0) }.is_ok());
    assert!(work.cancel_pending());
    // SAFETY: replacement is executed below before pinned storage is released.
    assert!(unsafe { next.accept(&work, 2, 1) }.is_ok());
    assert!(first.claim(&work).is_none());
    assert_pending::<usize>(&first, &[]);
    assert_pending(&next, &[&work]);
    assert_eq!(first.state.lock().active, 0);
    let (receipt, token) = CompletionObserver::prepare();
    assert!(next.observe_batch(1, token).is_ok());
    next.claim(&work).unwrap().run();
    assert!(receipt.is_completed());
    assert_eq!(next.state.lock().active, 0);
}

/// Checks a cancel using an old route leaves replacement resources and counts intact.
///
/// # Panics
/// Panics if stale detachment removes replacement Work or completes its batch.
#[def_test]
fn stale_partition_cannot_cancel_replacement() {
    let first = Arc::new(QueuePartition::new(
        kcpu_id_map::LogicalCpuId::new(0),
        NonZeroUsize::new(1).unwrap(),
    ));
    let next = Arc::new(QueuePartition::new(
        kcpu_id_map::LogicalCpuId::new(0),
        NonZeroUsize::new(1).unwrap(),
    ));
    let work = pin!(Work::new());
    // SAFETY: pinned storage remains alive through both synchronous cancellations.
    assert!(unsafe { first.accept(&work, 1, 0) }.is_ok());
    assert!(work.cancel_pending());
    // SAFETY: replacement is canceled below before pinned storage is released.
    assert!(unsafe { next.accept(&work, 2, 1) }.is_ok());
    let (receipt, token) = CompletionObserver::prepare();
    assert!(next.observe_batch(1, token).is_ok());
    assert!(
        first
            .detach_pending(&work, || panic!("stale route action"))
            .is_none()
    );
    assert_pending::<usize>(&first, &[]);
    assert_pending(&next, &[&work]);
    assert_eq!(work.observer_count(), 1);
    assert!(!receipt.is_completed());
    assert!(work.cancel_pending());
    assert!(receipt.is_completed());
    assert!(!work.cancel_pending());
}

/// Checks shared allowance limits execution, not acceptance of distinct Work.
///
/// # Panics
/// Panics if active limits are bypassed or cleanup fails to return capacity.
#[def_test]
fn partition_allowance_is_shared_and_reusable() {
    let partition = Arc::new(QueuePartition::new(
        kcpu_id_map::LogicalCpuId::new(0),
        NonZeroUsize::new(2).unwrap(),
    ));
    let first = pin!(Work::new());
    let second = pin!(Work::new());
    let third = pin!(Work::new());
    // SAFETY: each pinned Work remains live until its execution is retired below.
    assert!(unsafe { partition.accept(&first, 1, 0) }.is_ok());
    // SAFETY: each pinned Work remains live until its execution is retired below.
    assert!(unsafe { partition.accept(&second, 2, 0) }.is_ok());
    // SAFETY: each pinned Work remains live until its execution is retired below.
    assert!(unsafe { partition.accept(&third, 3, 0) }.is_ok());
    let one = first.claim().unwrap();
    let two = second.claim().unwrap();
    assert!(third.claim().is_none());
    one.run();
    third.claim().unwrap().run();
    assert_eq!(partition.state.lock().active, 1);
    two.run();
    assert_eq!(partition.state.lock().active, 0);
}

/// Checks actual Work admission, cancellation and claim keep one ordered membership.
///
/// # Panics
/// Panics if rejection changes order, middle cancellation loses neighbors, or
/// cancellation/claim leaves stale links behind.
#[def_test]
fn pending_membership_tracks_slot_transitions() {
    let partition = Arc::new(QueuePartition::new(
        kcpu_id_map::LogicalCpuId::new(0),
        NonZeroUsize::new(1).unwrap(),
    ));
    let first = pin!(Work::new());
    let middle = pin!(Work::new());
    let last = pin!(Work::new());
    // SAFETY: all three pinned objects remain live until drained below.
    assert!(unsafe { partition.accept(&first, 1, 0) }.is_ok());
    // SAFETY: same retained pinned objects, without concurrent callers.
    assert!(unsafe { partition.accept(&middle, 2, 0) }.is_ok());
    // SAFETY: same retained pinned objects, without concurrent callers.
    assert!(unsafe { partition.accept(&last, 3, 0) }.is_ok());
    assert_pending(&partition, &[&first, &middle, &last]);
    // SAFETY: duplicate submission uses the same stable retained object.
    assert_eq!(unsafe { partition.accept(&middle, 4, 1) }, Err(4));
    assert_pending(&partition, &[&first, &middle, &last]);
    assert!(middle.cancel_pending());
    assert_pending(&partition, &[&first, &last]);
    // SAFETY: replacement is also canceled before pinned storage is released.
    assert!(unsafe { partition.accept(&middle, 5, 1) }.is_ok());
    assert_pending(&partition, &[&first, &last, &middle]);
    let execution = first.claim().unwrap();
    assert_pending(&partition, &[&last, &middle]);
    assert!(last.claim().is_none());
    assert_pending(&partition, &[&last, &middle]);
    execution.run();
    assert!(last.cancel_pending());
    assert!(middle.cancel_pending());
    assert_pending::<usize>(&partition, &[]);
}

/// Checks a running predecessor blocks claim without removing its pending successor.
///
/// # Panics
/// Panics if a rejected successor claim unlinks it, or retirement changes its position.
#[def_test]
fn running_successor_stays_linked_until_claim() {
    let partition = Arc::new(QueuePartition::new(
        kcpu_id_map::LogicalCpuId::new(0),
        NonZeroUsize::new(2).unwrap(),
    ));
    let work = pin!(Work::new());
    let other = pin!(Work::new());
    // SAFETY: both pinned objects outlive all execution and pending accesses below.
    assert!(unsafe { partition.accept(&work, 1, 0) }.is_ok());
    let old = work.claim().unwrap();
    assert_pending::<usize>(&partition, &[]);
    // SAFETY: successor runs to completion before releasing the pinned object.
    assert!(unsafe { partition.accept(&work, 2, 1) }.is_ok());
    // SAFETY: other also runs to completion before releasing its pinned object.
    assert!(unsafe { partition.accept(&other, 3, 0) }.is_ok());
    assert_pending(&partition, &[&other, &work]);
    assert!(work.claim().is_none());
    assert_pending(&partition, &[&other, &work]);
    other.claim().unwrap().run();
    assert_pending(&partition, &[&work]);
    old.run();
    assert_pending(&partition, &[&work]);
    work.claim().unwrap().run();
    assert_pending::<usize>(&partition, &[]);
}

/// Checks one FIFO selects different concrete callback/resource layouts correctly.
///
/// # Panics
/// Panics on incorrect projection, order, cancellation or an extra execution.
#[def_test]
fn selection_handles_mixed_work_types() {
    let partition = Arc::new(QueuePartition::new(
        kcpu_id_map::LogicalCpuId::new(0),
        NonZeroUsize::new(1).unwrap(),
    ));
    let order = AtomicUsize::new(0);
    let first = pin!(Work::with_callback(|| {
        assert_eq!(order.fetch_add(1, Ordering::Relaxed), 0);
    }));
    let canceled = pin!(Work::with_callback(|| panic!("canceled callback ran")));
    let padding = [7u64; 32];
    let second = pin!(Work::with_callback(|| {
        core::hint::black_box(padding);
        assert_eq!(order.fetch_add(1, Ordering::Relaxed), 1);
    }));
    // SAFETY: all pinned Work and captures remain live through drain below.
    assert!(unsafe { partition.accept(&first, 1u8, 0) }.is_ok());
    // SAFETY: cancellation below drains this pinned Work before storage release.
    assert!(unsafe { partition.accept(&canceled, "cancel me", 0) }.is_ok());
    // SAFETY: second has a different resource layout but the same stable lifetime.
    assert!(unsafe { partition.accept(&second, [0u64; 16], 1) }.is_ok());
    assert!(canceled.cancel_pending());
    let first_run = partition.claim_next().unwrap();
    assert!(!first.cancel_pending());
    assert!(partition.claim_next().is_none());
    first_run.run();
    partition.claim_next().unwrap().run();
    assert!(partition.claim_next().is_none());
    assert_eq!(order.load(Ordering::Relaxed), 2);
    assert_eq!(partition.state.lock().active, 0);
}

/// Checks selection skips running predecessors without moving their successors back.
///
/// # Panics
/// Panics if blocked Work consumes allowance, disappears or loses FIFO position.
#[def_test]
fn selection_preserves_blocked_successor_position() {
    let partition = Arc::new(QueuePartition::new(
        kcpu_id_map::LogicalCpuId::new(0),
        NonZeroUsize::new(2).unwrap(),
    ));
    let order = AtomicUsize::new(0);
    let first = pin!(Work::with_callback(|| {
        order
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| Some(n * 10 + 1))
            .unwrap();
    }));
    let second = pin!(Work::with_callback(|| {
        order
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| Some(n * 10 + 2))
            .unwrap();
    }));
    let third = pin!(Work::with_callback(|| {
        order
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| Some(n * 10 + 3))
            .unwrap();
    }));
    // SAFETY: first stays pinned through both of its executions below.
    assert!(unsafe { partition.accept(&first, (), 0) }.is_ok());
    let old = partition.claim_next().unwrap();
    // SAFETY: successor shares the same retained pinned storage.
    assert!(unsafe { partition.accept(&first, (), 1) }.is_ok());
    // SAFETY: second is retained and drained before storage release.
    assert!(unsafe { partition.accept(&second, 2u8, 0) }.is_ok());
    let next = partition.claim_next().unwrap();
    assert!(partition.claim_next().is_none());
    // SAFETY: third is retained and drained before storage release.
    assert!(unsafe { partition.accept(&third, [3u64; 2], 0) }.is_ok());
    old.run();
    next.run();
    partition.claim_next().unwrap().run();
    partition.claim_next().unwrap().run();
    assert_eq!(order.load(Ordering::Relaxed), 1213);
    assert!(partition.claim_next().is_none());
    assert_eq!(partition.state.lock().active, 0);
}

/// Checks automatic selection cannot start a successor during predecessor cleanup.
///
/// # Panics
/// Panics on early successor selection, completion or missing final execution.
#[def_test]
fn selection_waits_for_resource_cleanup() {
    let partition = Arc::new(QueuePartition::new(
        kcpu_id_map::LogicalCpuId::new(0),
        NonZeroUsize::new(2).unwrap(),
    ));
    let calls = AtomicUsize::new(0);
    let work = pin!(Work::with_callback(|| {
        calls.fetch_add(1, Ordering::Relaxed);
    }));
    let (receipt, token) = CompletionObserver::prepare();
    let cleanup_receipt = receipt.clone();
    let cleanup_partition = partition.clone();
    let resource = Resource(Some(Box::new(move || {
        assert!(!cleanup_receipt.is_completed());
        assert!(cleanup_partition.claim_next().is_none());
        assert_eq!(cleanup_partition.state.lock().active, 1);
    })));
    // SAFETY: pinned Work outlives its original and successor execution.
    assert!(unsafe { partition.accept(&work, resource, 0) }.is_ok());
    assert!(partition.observe_batch(0, token).is_ok());
    let old = partition.claim_next().unwrap();
    // SAFETY: the same pinned Work is retained until successor completion below.
    assert!(unsafe { partition.accept(&work, Resource(None), 1) }.is_ok());
    old.run();
    assert!(receipt.is_completed());
    partition.claim_next().unwrap().run();
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    assert!(partition.claim_next().is_none());
}

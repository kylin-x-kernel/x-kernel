// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use unittest::{assert, assert_eq, def_test};

use super::{WorkSlot, WorkState};
use crate::work::observers::CompletionObserver;

fn partition() -> Arc<crate::queue::partition::QueuePartition> {
    Arc::new(crate::queue::partition::QueuePartition::new(
        0.into(),
        core::num::NonZeroUsize::new(1).unwrap(),
    ))
}

/// Timer-only acceptance owns resources and observations, but no queue contribution.
///
/// # Panics
/// Panics on premature eligibility, lost ownership or early observer completion.
#[def_test]
fn timer_pending_has_no_queue_accounting_or_allowance() {
    let partition = partition();
    let drops = Arc::new(AtomicUsize::new(0));
    let mut state = WorkState::new();
    state.set_delayed_pending(Resource(drops.clone()), &partition, 10);
    assert!(state.has_pending());
    assert!(state.pending_on(&partition));
    assert!(!state.can_accept_on(&partition));
    assert_eq!(state.delayed_deadline_ns(), Some(10));
    assert!(!state.release_pending());
    assert!(!state.claim());
    let (receipt, observer) = CompletionObserver::prepare();
    assert!(state.observe_pending(observer).is_ok());
    let (queue_receipt, queue_observer) = CompletionObserver::prepare();
    // Retaining a partition is not an ordinary in-flight contribution.
    let unused = partition.observe_batch(0, queue_observer).err().unwrap();
    drop(unused);
    assert!(!queue_receipt.is_completed());
    let canceled = state.cancel_pending().unwrap();
    assert_eq!(canceled.color, None);
    assert_eq!(state.delayed_deadline_ns(), None);
    assert!(!receipt.is_completed());
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    canceled.complete();
    assert!(receipt.is_completed());
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    assert_eq!(Arc::strong_count(&partition), 1);
}

/// Retiming preserves the same acceptance and observations through queue transfer.
///
/// # Panics
/// Panics if retime releases resources, loses observers or admits execution early.
#[def_test]
fn retime_and_transfer_keep_the_pending_occurrence() {
    let partition = partition();
    let drops = Arc::new(AtomicUsize::new(0));
    let mut state = WorkState::new();
    state.set_delayed_pending(Resource(drops.clone()), &partition, 10);
    let (receipt, observer) = CompletionObserver::prepare();
    assert!(state.observe_pending(observer).is_ok());
    state.retime_pending(20);
    state.retime_pending(5);
    assert_eq!(state.delayed_deadline_ns(), Some(5));
    assert_eq!(state.observer_count(), 1);
    assert!(!receipt.is_completed());
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    // State-only fixture: models the partition's already-reserved color and
    // list insertion. Do not call slot.complete without real accounting.
    state.queue_delayed_pending(2);
    assert_eq!(state.delayed_deadline_ns(), None);
    assert!(!state.claim());
    assert!(state.release_pending());
    assert!(!state.release_pending());
    assert!(state.claim());
    drop(state.take_running_resources());
    let finished = state.finish_after_cleanup().unwrap();
    assert_eq!(finished.color, Some(2));
    finished.observers.complete();
    assert!(receipt.is_completed());
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}

/// Expiry does not let a successor run before its predecessor's cleanup retires.
///
/// # Panics
/// Panics if timer transfer bypasses running ownership or releases allowance early.
#[def_test]
fn transferred_timer_waits_for_running_predecessor() {
    let partition = partition();
    let mut state = WorkState::new();
    // State-only fixture: no intrusive queue or real count is installed here.
    state.set_pending(1, &partition, 0);
    assert!(state.release_pending());
    assert!(state.claim());
    state.set_delayed_pending(2, &partition, 10);
    state.queue_delayed_pending(1);
    assert!(!state.release_pending());
    assert!(!state.claim());
    assert_eq!(state.take_running_resources(), Some(1));
    assert!(!state.release_pending());
    assert!(state.finish_after_cleanup().is_some());
    assert!(state.release_pending());
    assert!(state.claim());
    assert_eq!(state.take_running_resources(), Some(2));
    assert!(state.finish_after_cleanup().is_some());
}

/// Detached timer cleanup cannot complete observers of a replacement acceptance.
///
/// # Panics
/// Panics if old cleanup mutates replacement placement or follows its observers.
#[def_test]
fn canceled_timer_cleanup_is_independent_of_replacement() {
    let partition = partition();
    let drops = Arc::new(AtomicUsize::new(0));
    let mut state = WorkState::new();
    state.set_delayed_pending(Resource(drops.clone()), &partition, 10);
    let (old, observer) = CompletionObserver::prepare();
    assert!(state.observe_pending(observer).is_ok());
    let canceled = state.cancel_pending().unwrap();
    state.set_delayed_pending(Resource(drops.clone()), &partition, 20);
    let (new, observer) = CompletionObserver::prepare();
    assert!(state.observe_pending(observer).is_ok());
    canceled.complete();
    assert!(old.is_completed());
    assert!(!new.is_completed());
    assert_eq!(state.delayed_deadline_ns(), Some(20));
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    state.cancel_pending().unwrap().complete();
    assert!(new.is_completed());
    assert_eq!(drops.load(Ordering::Relaxed), 2);
    assert_eq!(Arc::strong_count(&partition), 1);
}

struct Resource(Arc<AtomicUsize>);

impl Drop for Resource {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

/// Checks the original four-state transition contract using actual occupied slots.
///
/// # Panics
/// Panics on a wrong result or slot occupancy for any state/operation pair.
#[def_test]
fn transition_table() {
    // Bits are test-only observations, never separately stored production state.
    for initial in 0..4 {
        for operation in 0..4 {
            let mut slots = WorkState::new();
            if initial & 2 != 0 {
                assert!(slots.accept(1).is_ok());
                assert!(slots.claim());
            }
            if initial & 1 != 0 {
                assert!(slots.accept(2).is_ok());
            }
            let (succeeded, expected) = match operation {
                0 => (slots.accept(3).is_ok(), (initial & 1 == 0, initial | 1)),
                1 => (
                    slots.claim(),
                    (initial == 1, if initial == 1 { 2 } else { initial }),
                ),
                2 => (
                    slots.cancel_pending().is_some(),
                    (initial & 1 != 0, initial & 2),
                ),
                _ => {
                    let _ = slots.take_running_resources();
                    (
                        slots.finish_after_cleanup().is_some(),
                        (initial & 2 != 0, initial & 1),
                    )
                }
            };
            assert_eq!(succeeded, expected.0);
            let occupancy =
                usize::from(slots.pending.is_some()) | (usize::from(slots.running.is_some()) << 1);
            assert_eq!(occupancy, expected.1);
        }
    }
}

/// Checks ownership transfer, cleanup occupancy, and observer capture during cleanup.
///
/// # Panics
/// Panics if claim drops resources, cleanup admits a successor, or completion is early.
#[def_test]
fn execution_keeps_slot_occupied_through_resource_cleanup() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut slots = WorkState::new();
    let (old, observer) = CompletionObserver::prepare();
    assert!(slots.accept(Resource(drops.clone())).is_ok());
    assert!(slots.observe_pending(observer).is_ok());
    assert!(slots.claim());
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    assert!(slots.finish_after_cleanup().is_none());
    let (next, observer) = CompletionObserver::prepare();
    assert!(slots.accept(Resource(drops.clone())).is_ok());
    assert!(slots.observe_pending(observer).is_ok());
    let resources = slots.take_running_resources().unwrap();
    assert!(slots.take_running_resources().is_none());
    assert!(!slots.claim());
    let (during_cleanup, observer) = CompletionObserver::prepare();
    assert!(slots.observe_running(observer).is_ok());
    assert!(!old.is_completed());
    drop(resources);
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    assert!(!slots.claim());
    let completed = slots.finish_after_cleanup().unwrap();
    assert!(!old.is_completed());
    assert!(slots.claim());
    completed.observers.complete();
    assert!(old.is_completed());
    assert!(during_cleanup.is_completed());
    assert!(!next.is_completed());
    drop(slots.take_running_resources().unwrap());
    slots.finish_after_cleanup().unwrap().observers.complete();
    assert!(next.is_completed());
    assert_eq!(drops.load(Ordering::Relaxed), 2);
}

/// Checks canceled resources/observers stay independent from replacement pending work.
///
/// # Panics
/// Panics if cancellation releases resources internally or completes replacement work.
#[def_test]
fn canceled_pending_payload_is_independent_of_replacement() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut slots = WorkState::new();
    assert!(slots.accept(Resource(drops.clone())).is_ok());
    assert!(slots.claim());
    assert!(slots.accept(Resource(drops.clone())).is_ok());
    let (old, observer) = CompletionObserver::prepare();
    assert!(slots.observe_pending(observer).is_ok());
    let WorkSlot {
        resources,
        observers,
        ..
    } = slots.cancel_pending().unwrap();
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    assert!(slots.accept(Resource(drops.clone())).is_ok());
    let (next, observer) = CompletionObserver::prepare();
    assert!(slots.observe_pending(observer).is_ok());
    assert!(!slots.claim());
    drop(resources);
    observers.complete();
    assert!(old.is_completed());
    assert!(!next.is_completed());
    drop(slots.take_running_resources().unwrap());
    slots.finish_after_cleanup().unwrap().observers.complete();
    let WorkSlot {
        resources,
        observers,
        ..
    } = slots.cancel_pending().unwrap();
    drop(resources);
    observers.complete();
    assert!(next.is_completed());
    assert_eq!(drops.load(Ordering::Relaxed), 3);
}

/// Checks rejected inputs return intact for cleanup or retry outside protection.
///
/// # Panics
/// Panics if failed acceptance drops resources or failed capture consumes its token.
#[def_test]
fn rejection_preserves_resource_and_observer_ownership() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut slots = WorkState::new();
    let (receipt, observer) = CompletionObserver::prepare();
    let observer = slots.observe_pending(observer).err().unwrap();
    let observer = slots.observe_running(observer).err().unwrap();
    assert!(slots.accept(Resource(drops.clone())).is_ok());
    assert!(slots.observe_pending(observer).is_ok());
    let rejected = slots.accept(Resource(drops.clone())).err().unwrap();
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    drop(rejected);
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    let WorkSlot {
        resources,
        observers,
        ..
    } = slots.cancel_pending().unwrap();
    assert!(!receipt.is_completed());
    drop(resources);
    observers.complete();
    assert!(receipt.is_completed());
    assert_eq!(drops.load(Ordering::Relaxed), 2);
}

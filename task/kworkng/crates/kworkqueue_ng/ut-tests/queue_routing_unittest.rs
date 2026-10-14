// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::{assert, assert_eq, cell::Cell, pin::pin};

use kcpu_id_map::LogicalCpuId;
use unittest::def_test;

use super::super::{
    Queue,
    config::{CpuPolicy, QueueConfig},
    partition::AdmissionResult,
    submission::SubmitError,
};
use crate::work::Work;

fn cpu(id: usize) -> LogicalCpuId {
    LogicalCpuId::new(id)
}

fn queue(policy: CpuPolicy) -> Arc<Queue> {
    Queue::new(QueueConfig::validate(&[cpu(4), cpu(0), cpu(2)], policy, 2, |_| true).unwrap())
}

/// Checks successful automatic submissions alone advance each Queue's RR sequence.
///
/// # Panics
/// Panics on wrong numeric order, rejection side effects or shared queue cursors.
#[def_test]
fn accepted_round_robin_advances_without_rejection_or_explicit_side_effects() {
    let queue = queue(CpuPolicy::RoundRobin);
    let other = self::queue(CpuPolicy::RoundRobin);
    let work = pin!(Work::with_callback(|| {}));
    let explicit = pin!(Work::with_callback(|| {}));
    for id in [0, 2, 4, 0, 2] {
        assert_eq!(
            // SAFETY: both Work objects remain pinned through all cancellations below.
            unsafe { queue.submit(&work, None, || panic!("RR must not sample CPU")) },
            Ok(cpu(id))
        );
        assert_eq!(
            // SAFETY: rejected duplicate cannot publish another occurrence.
            unsafe { queue.submit(&work, None, || panic!("owner reuse")) },
            Err(SubmitError::AlreadyQueued)
        );
        assert_eq!(
            // SAFETY: explicit Work stays pinned through the immediate cancellation.
            unsafe { queue.submit(&explicit, Some(cpu(4)), || panic!("explicit")) },
            Ok(cpu(4))
        );
        assert!(explicit.cancel_pending());
        assert_eq!(
            // SAFETY: disallowed target leaves pinned idle Work unchanged.
            unsafe { queue.submit(&explicit, Some(cpu(3)), || panic!("explicit")) },
            Err(SubmitError::InvalidCpu(cpu(3)))
        );
        assert!(work.cancel_pending());
    }
    assert_eq!(
        // SAFETY: the same idle Work remains pinned through cancellation on the other Queue.
        unsafe { other.submit(&work, None, || panic!("RR")) },
        Ok(cpu(0))
    );
    assert!(work.cancel_pending());
    queue.close_admission();
    assert_eq!(
        // SAFETY: closure rejects while Work remains pinned.
        unsafe { queue.submit(&work, None, || panic!("closed")) },
        Err(SubmitError::Closed)
    );
    assert_eq!(Arc::strong_count(&queue), 1);
}

/// Checks implicit running-owner reuse and explicit queue/CPU conflict rejection.
///
/// # Panics
/// Panics if a successor migrates, samples caller CPU or consumes a rotation turn.
#[def_test]
fn unfinished_owner_reuse_does_not_advance_round_robin() {
    let queue = queue(CpuPolicy::RoundRobin);
    let other = self::queue(CpuPolicy::RoundRobin);
    let work = pin!(Work::with_callback(|| {}));
    let next = pin!(Work::with_callback(|| {}));
    // SAFETY: pinned Work survives predecessor and successor execution below.
    assert_eq!(unsafe { queue.submit(&work, None, || cpu(4)) }, Ok(cpu(0)));
    let running = queue.claim_with_demand(cpu(0)).0.unwrap();
    assert_eq!(
        // SAFETY: rejected explicit conflict cannot alter retained running Work.
        unsafe { queue.submit(&work, Some(cpu(2)), || cpu(4)) },
        Err(SubmitError::OwnerConflict)
    );
    assert_eq!(
        // SAFETY: same retained storage; another Queue must reject unfinished ownership.
        unsafe { other.submit(&work, None, || cpu(0)) },
        Err(SubmitError::OwnerConflict)
    );
    assert_eq!(
        // SAFETY: accepted successor is drained after its retained predecessor below.
        unsafe { queue.submit(&work, None, || panic!("must reuse owner")) },
        Ok(cpu(0))
    );
    // SAFETY: next stays pinned until cancellation.
    assert_eq!(unsafe { queue.submit(&next, None, || cpu(4)) }, Ok(cpu(2)));
    assert!(next.cancel_pending());
    running.run();
    queue.claim_with_demand(cpu(0)).0.unwrap().run();
    assert_eq!(
        // SAFETY: after complete retirement the same retained Work may change Queue/CPU.
        unsafe { other.submit(&work, Some(cpu(4)), || cpu(0)) },
        Ok(cpu(4))
    );
    assert!(work.cancel_pending());
}

/// Checks selection-time current CPU sampling and rejection without fallback.
///
/// # Panics
/// Panics if target follows later migration, implicit invalid CPU falls back, or work leaks.
#[def_test]
fn current_cpu_is_sampled_at_selection_without_fallback() {
    let queue = queue(CpuPolicy::CurrentCpu);
    let work = pin!(Work::with_callback(|| {}));
    let current = Cell::new(cpu(2));
    assert_eq!(
        // SAFETY: pinned storage remains live through cancellation; sampler only changes a fixture.
        unsafe { queue.submit(&work, None, || current.replace(cpu(4))) },
        Ok(cpu(2))
    );
    assert_eq!(current.get(), cpu(4));
    assert!(work.cancel_pending());
    assert_eq!(
        // SAFETY: invalid selection leaves the retained pinned Work idle.
        unsafe { queue.submit(&work, None, || cpu(3)) },
        Err(SubmitError::InvalidCpu(cpu(3)))
    );
    assert_eq!(work.observer_count(), 0);
}

/// Checks an owner appearing during selection is revalidated, not overwritten.
///
/// # Panics
/// Panics if the outer attempt ignores the concurrent owner or corrupts membership.
#[def_test]
fn owner_appearing_during_selection_retries_and_preserves_inner_admission() {
    let queue = queue(CpuPolicy::CurrentCpu);
    let work = pin!(Work::with_callback(|| {}));
    let samples = Cell::new(0);
    let sample = || {
        samples.set(samples.get() + 1);
        assert_eq!(
            // SAFETY: same pinned storage; this simulates another admission before route commit.
            unsafe { queue.submit(&work, Some(cpu(0)), || panic!("explicit")) },
            Ok(cpu(0))
        );
        cpu(2)
    };
    // SAFETY: outer and injected inner submission share pinned storage drained below.
    let result = unsafe { queue.submit(&work, None, sample) };
    assert_eq!(result, Err(SubmitError::AlreadyQueued));
    assert_eq!(samples.get(), 1);
    assert!(queue.claim_with_demand(cpu(2)).0.is_none());
    queue.claim_with_demand(cpu(0)).0.unwrap().run();
    assert_eq!(Arc::strong_count(&queue), 1);
}

/// Checks both directions of stale route snapshots leave no published occurrence.
///
/// # Panics
/// Panics if a removed or newly established owner is accepted using a stale snapshot.
#[def_test]
fn admission_revalidates_owner_presence_before_any_effect() {
    let queue = queue(CpuPolicy::RoundRobin);
    let work = pin!(Work::with_callback(|| {}));
    assert_eq!(
        // SAFETY: Work remains pinned throughout all attempts and cancellation.
        unsafe { queue.submit(&work, Some(cpu(0)), || cpu(0)) },
        Ok(cpu(0))
    );
    let owner = work.state.lock().owner_partition().unwrap();
    assert!(matches!(
        // SAFETY: this Queue owns owner; pinned Work outlives this rejected stale attempt.
        unsafe { owner.accept_from_queue(&queue, &work, None) },
        AdmissionResult::Retry
    ));
    assert!(work.cancel_pending());
    assert!(matches!(
        // SAFETY: same owned partition/storage; only the expected owner snapshot is stale.
        unsafe { owner.accept_from_queue(&queue, &work, Some(&owner)) },
        AdmissionResult::Retry
    ));
    assert_eq!(work.observer_count(), 0);
    assert_eq!(Arc::strong_count(&queue), 1);
    assert_eq!(queue.select_cpu(None, || cpu(4)).unwrap(), cpu(0));
}

/// Checks concurrent RR selections advance once per acceptance, without rollback.
///
/// # Panics
/// Panics if co-selected CPUs, wrap, rejection or reverse completion corrupts the cursor.
#[def_test]
fn overlapping_round_robin_selections_preserve_acceptance_count() {
    for accepts_first in [false, true] {
        for accepts_second in [false, true] {
            for reverse in [false, true] {
                let queue = queue(CpuPolicy::RoundRobin);
                for _ in 0..2 {
                    queue.advance_round_robin();
                }
                let first = queue.select_cpu(None, || cpu(0)).unwrap();
                let second = queue.select_cpu(None, || cpu(0)).unwrap();
                assert_eq!(first, cpu(4));
                assert_eq!(second, cpu(4));
                let turns = if reverse {
                    [(second, accepts_second), (first, accepts_first)]
                } else {
                    [(first, accepts_first), (second, accepts_second)]
                };
                let mut index = 2;
                for (_cpu, accepted) in turns {
                    if accepted {
                        queue.advance_round_robin();
                        index = (index + 1) % 3;
                    }
                    assert_eq!(
                        queue.select_cpu(None, || cpu(0)).unwrap(),
                        [cpu(0), cpu(2), cpu(4)][index]
                    );
                }
            }
        }
    }
}

/// Checks disable during selection rejects before admission and RR advances on success only.
///
/// # Panics
/// Panics if disabled Work is published, canceled work is restored or rejection consumes a turn.
#[def_test]
fn disable_rejects_final_admission_and_preserves_rotation() {
    let current = queue(CpuPolicy::CurrentCpu);
    let work = pin!(Work::with_callback(|| {}));
    let held = core::cell::RefCell::new(None);
    let sample = || {
        *held.borrow_mut() = Some(work.disable().0);
        cpu(0)
    };
    assert_eq!(
        // SAFETY: the Work is pinned through submission and all guards are released below.
        unsafe { current.submit(&work, None, sample) },
        Err(SubmitError::Disabled)
    );
    drop(held.into_inner());
    let rr = queue(CpuPolicy::RoundRobin);
    let (guard, revoked) = work.disable();
    assert!(!revoked);
    assert_eq!(
        // SAFETY: retained pinned storage stays disabled through this rejected attempt.
        unsafe { rr.submit(&work, None, || panic!("RR")) },
        Err(SubmitError::Disabled)
    );
    drop(guard);
    assert_eq!(
        // SAFETY: successful Work is canceled before pinned storage release.
        unsafe { rr.submit(&work, None, || panic!("RR")) },
        Ok(cpu(0))
    );
    assert!(work.cancel_pending());
}

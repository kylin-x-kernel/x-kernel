// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Deterministic lifecycle/capacity tests, not scheduler or task-release evidence.

use core::{assert, assert_eq, num::NonZeroUsize};

use ktime_types::{MonotonicInstant, TimeSpan};
use unittest::def_test;

use crate::{pool::WorkerPool, worker::WorkerState};

fn pool(limit: usize) -> WorkerPool {
    WorkerPool::new(NonZeroUsize::new(limit).unwrap())
}

fn at(ms: u64) -> MonotonicInstant {
    MonotonicInstant::from_span_since_origin(TimeSpan::from_millis(ms))
}

fn start(pool: &mut WorkerPool) -> usize {
    let worker = pool.reserve_worker().unwrap();
    pool.worker_prepared(worker);
    pool.worker_started(worker);
    worker
}

fn check_capacity(pool: &WorkerPool, expected: usize) {
    assert_eq!(pool.reserved_workers(), expected);
    assert_eq!(
        pool.workers
            .iter()
            .filter(|state| **state != WorkerState::Empty)
            .count(),
        expected
    );
    assert!(expected <= pool.workers.len());
}

/// Deadline planning and retirement agree on the retained idle baseline.
///
/// # Panics
/// Panics on a premature deadline, lost baseline or inconsistent retirement state.
#[def_test]
fn idle_deadline_matches_retirement_policy() {
    let mut pool = pool(3);
    let idle_for = TimeSpan::from_millis(10);
    assert_eq!(pool.next_idle_deadline(idle_for), None);
    let first = start(&mut pool);
    pool.park_worker(first, at(2));
    assert_eq!(pool.next_idle_deadline(idle_for), None);
    let second = start(&mut pool);
    pool.park_worker(second, at(5));
    assert_eq!(pool.next_idle_deadline(idle_for), Some(at(12)));
    assert!(!pool.request_retirement(first, at(11), idle_for));
    assert!(pool.request_retirement(first, at(12), idle_for));
    assert!(pool.has_retiring_workers());
    assert_eq!(pool.next_idle_deadline(idle_for), None);
    assert!(!pool.request_retirement(second, at(100), idle_for));
    pool.worker_exited(first);
    assert!(!pool.has_retiring_workers());
    let available = start(&mut pool);
    assert_eq!(pool.next_idle_deadline(idle_for), Some(at(15)));
    assert!(pool.request_retirement(second, at(15), idle_for));
    assert_eq!(pool.state(available), WorkerState::Available);
    assert_eq!(pool.next_idle_deadline(idle_for), None);
}

/// Timer-range and arithmetic overflow cannot panic or retire workers early.
///
/// # Panics
/// Panics if an unrepresentable deadline is returned or retirement is premature.
#[def_test]
fn idle_deadline_checks_timer_and_arithmetic_limits() {
    let mut pool = pool(2);
    let first = start(&mut pool);
    let second = start(&mut pool);
    let since = MonotonicInstant::from_span_since_origin(TimeSpan::from_nanos(u64::MAX - 1));
    pool.park_worker(first, since);
    pool.park_worker(second, since);
    assert_eq!(
        pool.next_idle_deadline(TimeSpan::from_nanos(1)),
        Some(MonotonicInstant::from_span_since_origin(
            TimeSpan::from_nanos(u64::MAX)
        ))
    );
    for idle_for in [TimeSpan::from_nanos(2), TimeSpan::MAX] {
        assert_eq!(pool.next_idle_deadline(idle_for), None);
        assert!(!pool.request_retirement(first, since, idle_for));
        assert_eq!(pool.reserved_workers(), 2);
    }
    assert!(pool.wake_worker(first));
    assert!(pool.begin_selection(first));
}

/// Creating/prepared/available/occupied slots all consume the same fixed budget.
///
/// # Panics
/// Panics on overcommit, premature release or an incorrect lifecycle transition.
#[def_test]
fn normal_capacity_is_reserved_before_preparation() {
    let mut pool = pool(2);
    let first = pool.reserve_worker().unwrap();
    check_capacity(&pool, 1);
    assert_eq!(pool.state(first), WorkerState::Creating);
    let second = pool.reserve_worker().unwrap();
    check_capacity(&pool, 2);
    assert_eq!(pool.reserve_worker(), None);
    pool.worker_prepared(first);
    assert_eq!(pool.state(first), WorkerState::Prepared);
    assert!(!pool.begin_selection(first));
    pool.worker_started(first);
    assert!(pool.begin_selection(first));
    pool.finish_selection(first, true);
    assert_eq!(pool.state(first), WorkerState::Occupied);
    assert_eq!(pool.state(second), WorkerState::Creating);
    assert_eq!(pool.reserve_worker(), None);
    check_capacity(&pool, 2);
}

/// A worker can advance before activation returns without a manager commit phase.
///
/// # Panics
/// Panics if startup/selection/cleanup overwrite state or return capacity early.
#[def_test]
fn worker_start_owns_progress_after_prepared_publication() {
    let mut pool = pool(1);
    let worker = start(&mut pool);
    assert!(pool.begin_selection(worker));
    assert!(!pool.begin_selection(worker));
    pool.finish_selection(worker, false);
    assert!(pool.begin_selection(worker));
    pool.finish_selection(worker, true);
    assert!(!pool.wake_worker(worker));
    assert!(!pool.begin_selection(worker));
    pool.finish_execution(worker);
    assert_eq!(pool.state(worker), WorkerState::Available);
    check_capacity(&pool, 1);
}

/// A positive one-worker limit preserves its baseline and saturates while occupied.
///
/// # Panics
/// Panics on baseline retirement or an extra creation reservation.
#[def_test]
fn one_worker_limit_keeps_the_baseline() {
    let mut pool = pool(1);
    let worker = start(&mut pool);
    pool.park_worker(worker, at(1));
    assert!(!pool.request_retirement(worker, at(100), TimeSpan::from_millis(10)));
    assert!(pool.wake_worker(worker));
    assert!(!pool.wake_worker(worker));
    assert!(pool.begin_selection(worker));
    pool.finish_selection(worker, true);
    assert_eq!(pool.reserve_worker(), None);
    assert!(!pool.request_retirement(worker, at(1000), TimeSpan::from_millis(10)));
    check_capacity(&pool, 1);
}

/// Continuous idle age restarts after wake and rejects reversed clock samples.
///
/// # Panics
/// Panics if time before the idle threshold permits retirement.
#[def_test]
fn wake_discards_old_idle_age() {
    let mut pool = pool(2);
    let worker = start(&mut pool);
    let _baseline = start(&mut pool);
    pool.park_worker(worker, at(10));
    let idle_for = TimeSpan::from_millis(5);
    assert!(!pool.request_retirement(worker, at(9), idle_for));
    assert!(!pool.request_retirement(worker, at(14), idle_for));
    assert!(pool.wake_worker(worker));
    assert!(!pool.request_retirement(worker, at(100), idle_for));
    pool.park_worker(worker, at(100));
    assert!(!pool.request_retirement(worker, at(104), idle_for));
    assert!(pool.request_retirement(worker, at(105), idle_for));
    assert_eq!(pool.state(worker), WorkerState::Retiring);
    check_capacity(&pool, 2);
}

/// Wake and retirement orders cannot make a retiring worker claim new execution.
///
/// # Panics
/// Panics if either ordering resurrects a retired slot or loses a wake reservation.
#[def_test]
fn wake_and_retirement_have_exclusive_outcomes() {
    for wake_first in [true, false] {
        let mut pool = pool(2);
        let worker = start(&mut pool);
        let _baseline = start(&mut pool);
        pool.park_worker(worker, at(0));
        if wake_first {
            assert!(pool.wake_worker(worker));
            assert!(!pool.request_retirement(worker, at(10), TimeSpan::from_millis(1)));
            assert!(pool.begin_selection(worker));
        } else {
            assert!(pool.request_retirement(worker, at(10), TimeSpan::from_millis(1)));
            assert!(!pool.wake_worker(worker));
            assert!(!pool.begin_selection(worker));
        }
        check_capacity(&pool, 2);
        assert_eq!(pool.reserve_worker(), None);
    }
}

/// Idle retirement preserves an occupied callback and a ready baseline.
///
/// Occupied deliberately has no scheduler-sleep transition. This verifies only
/// capacity bookkeeping; a runtime test must later hold an actual sleeping callback.
///
/// # Panics
/// Panics on occupied retirement, lost baseline or slot reuse before confirmed exit.
#[def_test]
fn idle_retirement_does_not_free_occupied_or_exiting_capacity() {
    let mut pool = pool(3);
    let occupied = start(&mut pool);
    let spare = start(&mut pool);
    let baseline = start(&mut pool);
    assert!(pool.begin_selection(occupied));
    pool.finish_selection(occupied, true);
    pool.park_worker(spare, at(0));
    pool.park_worker(baseline, at(0));
    let idle_for = TimeSpan::from_millis(1);
    assert!(!pool.request_retirement(occupied, at(100), idle_for));
    assert!(pool.request_retirement(spare, at(100), idle_for));
    assert!(!pool.request_retirement(baseline, at(100), idle_for));
    assert_eq!(pool.reserve_worker(), None);
    check_capacity(&pool, 3);
    pool.worker_exited(spare);
    check_capacity(&pool, 2);
    assert_eq!(pool.reserve_worker(), Some(spare));
    check_capacity(&pool, 3);
    assert_eq!(pool.state(occupied), WorkerState::Occupied);
}

/// Disposed unactivated attempts release capacity without a synthetic worker exit.
///
/// # Panics
/// Panics if a creating/prepared rollback leaks or duplicates a slot reservation.
#[def_test]
fn unstarted_disposal_releases_exactly_its_reservation() {
    let mut pool = pool(2);
    let creating = pool.reserve_worker().unwrap();
    let prepared = pool.reserve_worker().unwrap();
    pool.worker_prepared(prepared);
    pool.release_unstarted_worker(creating);
    check_capacity(&pool, 1);
    assert_eq!(pool.state(prepared), WorkerState::Prepared);
    pool.release_unstarted_worker(prepared);
    check_capacity(&pool, 0);
    assert_eq!(pool.reserve_worker(), Some(creating));
    assert_eq!(pool.reserve_worker(), Some(prepared));
    check_capacity(&pool, 2);
}

/// Another CPU owns independent capacity while this CPU's aggregate is saturated.
///
/// # Panics
/// Panics if reservations affect another pool or one owner exceeds its limit.
#[def_test]
fn distinct_capacity_owners_are_independent() {
    let mut cpu0 = pool(1);
    let mut cpu1 = pool(1);
    assert_eq!(cpu0.reserve_worker(), Some(0));
    assert_eq!(cpu0.reserve_worker(), None);
    check_capacity(&cpu1, 0);
    assert_eq!(cpu1.reserve_worker(), Some(0));
    check_capacity(&cpu0, 1);
    check_capacity(&cpu1, 1);
}

/// Growth waits for reserved startup/selection and requires demand once a baseline exists.
///
/// # Panics
/// Panics if a usable context requests another worker or occupied demand is ignored.
#[def_test]
fn growth_predicate_distinguishes_startup_selection_and_occupation() {
    let mut pool = pool(2);
    assert!(pool.needs_worker(false));
    let worker = pool.reserve_worker().unwrap();
    assert!(!pool.needs_worker(true));
    pool.worker_prepared(worker);
    assert!(!pool.needs_worker(true));
    pool.worker_started(worker);
    assert!(!pool.needs_worker(true));
    assert!(pool.begin_selection(worker));
    assert!(!pool.needs_worker(true));
    pool.finish_selection(worker, true);
    assert!(!pool.needs_worker(false));
    assert!(pool.needs_worker(true));
    pool.reserve_worker().unwrap();
    assert!(!pool.needs_worker(true));
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq};

use super::*;

#[unittest::def_test]
fn cleanup_requests_progress_only_for_the_last_observed_blocker() {
    for is_terminal in [false, true] {
        let original = ReaderNode::new();
        let destination = ReaderNode::new();
        let first = TaskReader::new();
        let last = TaskReader::new();
        let newer = TaskReader::new();
        let _guard = kspin::NoPreemptIrqSave::new();
        for reader in [&first, &last] {
            assert!(reader.try_enter());
            // SAFETY: isolated active fixture, fixed storage and exclusion;
            // cleanup below balances the synthetic scope without a live guard.
            unsafe { original.enroll(reader) };
        }
        assert!(original.begin_period(1));
        assert!(original.observe_cpu(1));
        assert!(newer.try_enter());
        // SAFETY: distinct isolated reader enters after the CPU observation.
        unsafe { original.enroll(&newer) };
        // SAFETY: serialized migration cannot change the last reader's owner.
        unsafe { destination.enroll(&last) };
        assert_eq!(first.try_leave(), Some(0));
        // SAFETY: inactive exclusive fixture, no node lock held.
        let first_exit = unsafe { first.finish_deferred_cleanup() };
        assert!(first_exit.had_participation);
        assert!(!first_exit.needs_progress);
        assert_eq!(original.ready_period(), None);
        let last_exit = if is_terminal {
            // SAFETY: synthetic abandoned scope has no continuing execution.
            unsafe { last.finish_task_exit() }
        } else {
            assert_eq!(last.try_leave(), Some(0));
            // SAFETY: exclusive inactive fixture under exclusion.
            unsafe { last.finish_deferred_cleanup() }
        };
        assert!(last_exit.had_participation);
        assert!(last_exit.needs_progress);
        assert_eq!(original.ready_period(), Some(1));
        assert_eq!(destination.reader_count(), 0);
        // The newer reader does not block this period and cannot notify twice.
        assert_eq!(newer.try_leave(), Some(0));
        // SAFETY: inactive exclusive fixture with retained original node.
        let newer_exit = unsafe { newer.finish_deferred_cleanup() };
        assert!(newer_exit.had_participation);
        assert!(!newer_exit.needs_progress);
        // SAFETY: repeated cleanup of the same stable inactive fixture.
        let repeated = unsafe { last.finish_deferred_cleanup() };
        assert_eq!(repeated, ReaderCleanupOutcome::default());
        assert!(original.finish_period(1));
    }
}

#[unittest::def_test]
fn nested_scopes_balance_on_error_return() {
    let reader = TaskReader::new();
    let (outer, inner, after_inner, result) = reader.with_read(|_| {
        let outer = reader.nesting.load(Ordering::Acquire);
        let inner = reader.with_read(|_| reader.nesting.load(Ordering::Acquire));
        let after_inner = reader.nesting.load(Ordering::Acquire);
        (outer, inner, after_inner, Err::<(), _>("scope result"))
    });
    assert_eq!((outer, inner, after_inner), (1, 2, 1));
    assert_eq!(result, Err("scope result"));
    assert!(!reader.is_reading());
}

#[unittest::def_test]
fn nesting_never_wraps() {
    let reader = TaskReader::new();
    assert_eq!(reader.try_leave(), None);
    assert_eq!(reader.nesting.load(Ordering::Acquire), 0);
    reader.nesting.store(usize::MAX - 1, Ordering::Relaxed);
    assert!(reader.try_enter());
    assert!(!reader.try_enter());
    assert_eq!(reader.nesting.load(Ordering::Acquire), usize::MAX);
    assert_eq!(reader.try_leave(), Some(usize::MAX - 1));
    assert_eq!(reader.nesting.load(Ordering::Acquire), usize::MAX - 1);
}

#[unittest::def_test]
fn deferred_exit_retains_blocker_until_original_node_cleanup() {
    let node = ReaderNode::new();
    let other = ReaderNode::new();
    let reader = TaskReader::new();
    let _guard = kspin::NoPreemptIrqSave::new();
    let enroll_fn = |_: &ReadGuard<'_>| {
        // SAFETY: this fixture exclusively enrolls its stable active reader.
        unsafe { node.enroll(&reader) };
    };
    // SAFETY: isolated fixture under exclusion; storage stays fixed through
    // the explicit cleanup below. Enrollment models a scheduler switch.
    unsafe { reader.with_read_deferred(enroll_fn) };
    assert!(!reader.is_reading());
    assert!(reader.is_enrolled_on(&node));
    assert!(node.begin_period(1));
    assert!(node.observe_cpu(1));
    assert_eq!(node.ready_period(), None);
    let reentered = reader.with_read(|_| {
        // SAFETY: serialized migration of the same stable fixture.
        unsafe { other.enroll(&reader) };
        // SAFETY: exclusive fixture, IRQ/preemption excluded, no node lock held.
        let cleaned = unsafe { reader.finish_deferred_cleanup() };
        (
            cleaned.had_participation,
            reader.is_enrolled_on(&node),
            node.ready_period(),
        )
    });
    assert_eq!(reentered, (false, true, None));
    assert!(!reader.is_enrolled());
    assert_eq!(node.ready_period(), Some(1));
    assert_eq!(other.reader_count(), 0);
    // SAFETY: inactive isolated fixture remains stable and excluded.
    assert!(!unsafe { reader.finish_deferred_cleanup() }.had_participation);
}

#[unittest::def_test]
fn deferred_cleanup_does_not_supply_cpu_observation() {
    let node = ReaderNode::new();
    let reader = TaskReader::new();
    let _guard = kspin::NoPreemptIrqSave::new();
    let enroll_fn = |_: &ReadGuard<'_>| {
        // SAFETY: stable active fixture under scheduler exclusion.
        unsafe { node.enroll(&reader) };
    };
    // SAFETY: exclusively driven fixture retained through cleanup below.
    unsafe { reader.with_read_deferred(enroll_fn) };
    assert!(node.begin_period(1));
    // SAFETY: reader is inactive; no interrupt or other task can change it.
    let outcome = unsafe { reader.finish_deferred_cleanup() };
    assert!(outcome.had_participation);
    assert!(!outcome.needs_progress);
    assert_eq!(node.reader_count(), 0);
    assert_eq!(node.ready_period(), None);
    assert!(node.observe_cpu(1));
    assert_eq!(node.ready_period(), Some(1));
    // Models an ordinary exit resuming after the scheduler already cleaned up.
    reader.remove_enrollment();
    assert!(!reader.is_enrolled());
}

#[unittest::def_test]
fn terminal_cleanup_removes_zero_depth_deferred_membership() {
    let node = ReaderNode::new();
    let reader = TaskReader::new();
    let _guard = kspin::NoPreemptIrqSave::new();
    let enroll_fn = |_: &ReadGuard<'_>| {
        // SAFETY: exclusive fixture, retained storage, scheduler exclusion.
        unsafe { node.enroll(&reader) };
    };
    // SAFETY: synthetic task has no live execution after the scope returns;
    // its reader and owner node stay stable through terminal cleanup.
    unsafe { reader.with_read_deferred(enroll_fn) };
    assert!(!reader.is_reading());
    assert!(reader.is_enrolled());
    // SAFETY: fixture is permanently inactive, with no live guard or interrupt.
    assert!(unsafe { reader.finish_task_exit() }.had_participation);
    assert_eq!(node.reader_count(), 0);
}

#[unittest::def_test]
fn terminal_cleanup_balances_unenrolled_nesting() {
    let reader = TaskReader::new();
    let (inactive, active, repeated) = {
        let _guard = kspin::NoPreemptIrqSave::new();
        // SAFETY: isolated stable fixture with no executing task or guard;
        // IRQ/preemption exclusion covers all terminal cleanup calls here.
        let inactive = unsafe { reader.finish_task_exit() };
        let entered = reader.try_enter() && reader.try_enter();
        // SAFETY: synthetic nesting has no guard or continuing read accesses.
        let active = unsafe { reader.finish_task_exit() };
        // SAFETY: the same exclusive fixture remains inactive and stable.
        let repeated = unsafe { reader.finish_task_exit() };
        (
            inactive.had_participation,
            entered && active.had_participation,
            repeated.had_participation,
        )
    };
    assert!(!inactive);
    assert!(active);
    assert!(!repeated);
    assert!(!reader.is_reading());
    assert!(!reader.is_enrolled());
}

#[unittest::def_test]
fn terminal_cleanup_unlinks_original_node_without_removing_other_readers() {
    let original = ReaderNode::new();
    let destination = ReaderNode::new();
    let exited = TaskReader::new();
    let survivor = TaskReader::new();
    let observed = {
        let _guard = kspin::NoPreemptIrqSave::new();
        // Synthetic nesting models an abandoned stack without leaking guards.
        let entered = exited.try_enter() && exited.try_enter();
        // SAFETY: exclusive fixture, stable storage, IRQ/preemption excluded;
        // terminal cleanup below removes enrollment before storage is dropped.
        unsafe { original.enroll(&exited) };
        // SAFETY: serialized migration attempt on the same retained fixture.
        unsafe { destination.enroll(&exited) };
        survivor.with_read(|_| {
            // SAFETY: distinct stable reader, exclusive fixture; its ordinary
            // outer exit removes membership before the node is destroyed.
            unsafe { original.enroll(&survivor) };
            // SAFETY: no actual task or guard can resume the exited fixture;
            // both nodes and the reader are retained and updates are exclusive.
            let cleaned = unsafe { exited.finish_task_exit() };
            (
                entered && cleaned.had_participation,
                original.reader_count(),
                destination.reader_count(),
                survivor.is_enrolled_on(&original),
            )
        })
    };
    assert_eq!(observed, (true, 1, 0, true));
    assert!(!exited.is_reading());
    assert!(!exited.is_enrolled());
    assert_eq!(original.reader_count(), 0);
}

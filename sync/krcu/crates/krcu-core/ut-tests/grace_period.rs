// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Deterministic node transactions, not hardware quiescent-state observations.

#[cfg(not(test))]
use unittest::{assert, assert_eq};

use super::*;
use crate::{PeriodId, grace_period::GracePeriodCoordinator};

// Private fixtures need overlapping scopes whose exits are not stack ordered,
// as with different preempted tasks. Production callers only borrow the token.
// This bypass tests reader accounting, not with_read's token escape restrictions.
fn enter(reader: &TaskReader) -> ReadGuard<'_> {
    core::assert!(reader.try_enter());
    ReadGuard {
        reader,
        can_unlink_on_exit: true,
        task_local: PhantomData,
    }
}

#[cfg_attr(test, test)]
#[cfg_attr(not(test), unittest::def_test)]
fn initialization_and_enrollment_in_either_order_preserve_old_reader() {
    for initialize_first in [false, true] {
        let node = ReaderNode::new();
        let reader = TaskReader::new();
        let observed = {
            let _guard = kspin::NoPreemptIrqSave::new();
            let outer = enter(&reader);
            let mut began = false;
            if initialize_first {
                began = node.begin_period(7);
            }
            // SAFETY: exclusive, stable fixture under scheduler exclusion;
            // outer guard unlinks the reader before either allocation ends.
            unsafe { node.enroll(&reader) };
            if !initialize_first {
                began = node.begin_period(7);
            }
            let acknowledged = node.observe_cpu(7);
            let inner = enter(&reader);
            drop(inner);
            let during = node.ready_period();
            let premature_finish = node.finish_period(7);
            drop(outer);
            (
                began,
                acknowledged,
                during,
                premature_finish,
                node.ready_period(),
            )
        };
        assert_eq!(observed, (true, true, None, false, Some(7)));
        assert_eq!(node.reader_count(), 0);
    }
}

#[cfg_attr(test, test)]
#[cfg_attr(not(test), unittest::def_test)]
fn reader_exit_does_not_discharge_the_cpu_obligation() {
    let node = ReaderNode::new();
    let reader = TaskReader::new();
    let observed = {
        let _guard = kspin::NoPreemptIrqSave::new();
        let scope = enter(&reader);
        // SAFETY: exclusive stable fixture; scope removes the borrowed link.
        unsafe { node.enroll(&reader) };
        let began = node.begin_period(3);
        drop(scope);
        let before_cpu = node.ready_period();
        let duplicate_start = node.begin_period(3) || node.begin_period(4);
        let stale_cpu = node.observe_cpu(2);
        let cpu = node.observe_cpu(3);
        (
            began,
            before_cpu,
            duplicate_start,
            stale_cpu,
            cpu,
            node.ready_period(),
        )
    };
    assert_eq!(observed, (true, None, false, false, true, Some(3)));
}

#[cfg_attr(test, test)]
#[cfg_attr(not(test), unittest::def_test)]
fn reader_removed_before_initialization_is_not_a_blocker() {
    let node = ReaderNode::new();
    let reader = TaskReader::new();
    let observed = {
        let _guard = kspin::NoPreemptIrqSave::new();
        let scope = enter(&reader);
        // SAFETY: exclusive stable fixture under scheduler exclusion.
        unsafe { node.enroll(&reader) };
        drop(scope);
        let began = node.begin_period(1);
        let before_cpu = node.ready_period();
        let cpu = node.observe_cpu(1);
        (began, before_cpu, cpu, node.ready_period())
    };
    assert_eq!(observed, (true, None, true, Some(1)));
}

#[cfg_attr(test, test)]
#[cfg_attr(not(test), unittest::def_test)]
fn fresh_readers_do_not_extend_current_period_but_block_the_next() {
    let node = ReaderNode::new();
    let old = TaskReader::new();
    let fresh = TaskReader::new();
    let transient = TaskReader::new();
    let observed = {
        let _guard = kspin::NoPreemptIrqSave::new();
        let old_scope = enter(&old);
        // SAFETY: isolated stable readers; guards end before fixture storage.
        unsafe { node.enroll(&old) };
        let began = node.begin_period(PeriodId::MAX);
        let cpu = node.observe_cpu(PeriodId::MAX);
        let fresh_scope = enter(&fresh);
        // SAFETY: distinct reader, same fixture lifetime and exclusion.
        unsafe { node.enroll(&fresh) };
        let mut churn_preserved_old = true;
        for _ in 0..32 {
            let scope = enter(&transient);
            // SAFETY: previous iteration unlinked this stable reader; exclusive
            // enrollment and matching scope destruction occur in this fixture.
            unsafe { node.enroll(&transient) };
            drop(scope);
            churn_preserved_old &= node.ready_period().is_none();
        }
        drop(old_scope);
        let ready_with_fresh = node.ready_period();
        let still_enrolled = node.reader_count();
        let stale_finish = node.finish_period(0);
        let finished = node.finish_period(PeriodId::MAX);
        let next_began = node.begin_period(0);
        let stale_cpu = node.observe_cpu(PeriodId::MAX);
        let next_cpu = node.observe_cpu(0);
        let next_waiting = node.ready_period();
        drop(fresh_scope);
        (
            began && cpu && churn_preserved_old,
            ready_with_fresh,
            still_enrolled,
            stale_finish,
            finished && next_began,
            stale_cpu,
            next_cpu,
            next_waiting,
            node.ready_period(),
        )
    };
    assert_eq!(
        observed,
        (
            true,
            Some(PeriodId::MAX),
            1,
            false,
            true,
            false,
            true,
            None,
            Some(0)
        )
    );
}

#[cfg_attr(test, test)]
#[cfg_attr(not(test), unittest::def_test)]
fn blocker_boundary_advances_without_waiting_for_new_list_head() {
    let node = ReaderNode::new();
    let first = TaskReader::new();
    let second = TaskReader::new();
    let third = TaskReader::new();
    let fresh = TaskReader::new();
    let observed = {
        let _guard = kspin::NoPreemptIrqSave::new();
        let began = node.begin_period(9);
        let first_scope = enter(&first);
        let second_scope = enter(&second);
        let third_scope = enter(&third);
        // SAFETY: all three readers have distinct links and stable storage,
        // are exclusively driven here, and are unlinked by their own guards.
        unsafe { node.enroll(&first) };
        // SAFETY: same retained fixture; second has its own scope and link.
        unsafe { node.enroll(&second) };
        // SAFETY: same retained fixture; third has its own scope and link.
        unsafe { node.enroll(&third) };
        let cpu = node.observe_cpu(9);
        let fresh_scope = enter(&fresh);
        // SAFETY: distinct stable reader, under the same exclusion as above.
        unsafe { node.enroll(&fresh) };
        drop(second_scope); // Interior blocker removal must not clear the boundary.
        let after_interior = node.ready_period();
        drop(first_scope); // Advance over the already-removed interior reader.
        let after_first = node.ready_period();
        drop(third_scope);
        let after_last = node.ready_period();
        let remaining = node.reader_count();
        drop(fresh_scope);
        (
            began && cpu,
            after_interior,
            after_first,
            after_last,
            remaining,
        )
    };
    assert_eq!(observed, (true, None, None, Some(9), 1));
}

#[cfg_attr(test, test)]
#[cfg_attr(not(test), unittest::def_test)]
fn terminal_exit_advances_the_original_nodes_blocker_boundary() {
    let original = ReaderNode::new();
    let destination = ReaderNode::new();
    let exited = TaskReader::new();
    let survivor = TaskReader::new();
    let observed = {
        let _guard = kspin::NoPreemptIrqSave::new();
        let began = original.begin_period(12) && destination.begin_period(12);
        // Model abandoned nesting without constructing guards that could later
        // run. Terminal cleanup below is the sole exit for this fixture.
        let entered = exited.try_enter() && exited.try_enter();
        // SAFETY: exclusive stable fixture under IRQ/preemption exclusion;
        // terminal cleanup occurs before this reader or either node is freed.
        unsafe { original.enroll(&exited) };
        // SAFETY: serial migration attempt preserves the original enrollment.
        unsafe { destination.enroll(&exited) };
        let survivor_scope = enter(&survivor);
        // SAFETY: survivor has a distinct stable link and an ordinary scope exit.
        unsafe { original.enroll(&survivor) };
        let cpu = original.observe_cpu(12) && destination.observe_cpu(12);
        // SAFETY: no task or guard can resume the exited fixture; nodes and
        // reader remain stable, all scope/enrollment mutations are exclusive.
        let cleaned = unsafe { exited.finish_task_exit() };
        let with_survivor = original.ready_period();
        let destination_ready = destination.ready_period();
        drop(survivor_scope);
        (
            began && entered && cpu && cleaned,
            with_survivor,
            destination_ready,
            original.ready_period(),
        )
    };
    assert_eq!(observed, (true, None, Some(12), Some(12)));
    assert!(!exited.is_enrolled());
}

#[cfg_attr(test, test)]
#[cfg_attr(not(test), unittest::def_test)]
fn withdrawal_waits_for_old_readers_without_waiting_for_later_versions() {
    for initialize_first in [false, true] {
        let nodes: [ReaderNode; 2] = core::array::from_fn(|_| ReaderNode::new());
        let gp = GracePeriodCoordinator::new(&nodes, [true, true]).expect("nonempty fixture");
        let old = TaskReader::new();
        let fresh = TaskReader::new();
        let transient = TaskReader::new();
        // A version selector models withdrawal, not a protected pointer or a
        // weak-memory publication proof. No object is freed by this fixture.
        let published = AtomicUsize::new(1);
        let observed = {
            let _guard = kspin::NoPreemptIrqSave::new();
            let old_scope = enter(&old);
            let old_version = published.load(Ordering::Acquire);
            published.store(2, Ordering::Release);
            let began = if initialize_first {
                gp.begin_period()
            } else {
                None
            };
            // SAFETY: stable, exclusively driven fixtures under scheduler
            // exclusion; every scope unlinks before reader/node storage ends.
            unsafe { nodes[0].enroll(&old) };
            let began = if initialize_first {
                began
            } else {
                gp.begin_period()
            };
            let cpus = gp.observe_cpu(0, 1) && gp.observe_cpu(1, 1);
            let peer = gp.report_node(1);
            let fresh_scope = enter(&fresh);
            let fresh_version = published.load(Ordering::Acquire);
            // SAFETY: distinct stable reader, same exclusive fixture lifetime.
            unsafe { nodes[0].enroll(&fresh) };
            // A second update does not wait for the first old reader. Its
            // retirement would need a full later period, tested below.
            published.store(3, Ordering::Release);
            let mut remained_blocked = true;
            for _ in 0..32 {
                let scope = enter(&transient);
                // SAFETY: the previous iteration removed this reader's link;
                // this scope ends before the stable fixture can be destroyed.
                unsafe { nodes[0].enroll(&transient) };
                drop(scope);
                remained_blocked &= !gp.report_node(0) && gp.try_complete_period().is_none();
            }
            drop(old_scope);
            let first_report = gp.report_node(0);
            let first_complete = gp.try_complete_period();
            let fresh_still_reading = fresh.is_reading() && fresh.is_enrolled();
            let second_begin = gp.begin_period();
            let second_cpus = gp.observe_cpu(0, 2) && gp.observe_cpu(1, 2);
            let second_peer = gp.report_node(1);
            let second_blocked = !gp.report_node(0) && gp.try_complete_period().is_none();
            drop(fresh_scope);
            let second_report = gp.report_node(0);
            (
                (
                    old_version,
                    fresh_version,
                    published.load(Ordering::Acquire),
                ),
                began,
                cpus && peer && remained_blocked && first_report,
                first_complete,
                fresh_still_reading,
                second_begin,
                second_cpus && second_peer && second_blocked && second_report,
                gp.try_complete_period(),
            )
        };
        assert_eq!(
            observed,
            (
                (1, 2, 3),
                Some(1),
                true,
                Some(1),
                true,
                Some(2),
                true,
                Some(2)
            )
        );
        assert_eq!(gp.completed_period(), 2);
        assert_eq!(nodes[0].reader_count(), 0);
        assert_eq!(nodes[1].reader_count(), 0);
    }
}

#[cfg_attr(test, test)]
#[cfg_attr(not(test), unittest::def_test)]
fn migrated_terminal_reader_releases_original_node_for_global_completion() {
    let nodes: [ReaderNode; 2] = core::array::from_fn(|_| ReaderNode::new());
    let gp = GracePeriodCoordinator::new(&nodes, [true, true]).expect("nonempty fixture");
    let exited = TaskReader::new();
    let observed = {
        let _guard = kspin::NoPreemptIrqSave::new();
        // No guard is constructed: model a task abandoning nested scopes,
        // with terminal cleanup as its only remaining participation exit.
        let entered = exited.try_enter() && exited.try_enter();
        // SAFETY: exclusive stable fixture; terminal cleanup below removes
        // the link before either node or reader storage is destroyed.
        unsafe { nodes[0].enroll(&exited) };
        let began = gp.begin_period();
        // SAFETY: serialized migration attempt, with unchanged stable storage
        // and no concurrent scope mutation or enrollment.
        unsafe { nodes[1].enroll(&exited) };
        let kept_original = exited.is_enrolled_on(&nodes[0]) && nodes[1].reader_count() == 0;
        let cpus = gp.observe_cpu(0, 1) && gp.observe_cpu(1, 1);
        let destination_report = gp.report_node(1);
        let original_blocked = !gp.report_node(0) && gp.try_complete_period().is_none();
        // SAFETY: models post-switch permanent exit; no guard or task can
        // resume, and IRQ/preemption exclusion and stable storage persist.
        let cleaned = unsafe { exited.finish_task_exit() };
        let original_report = gp.report_node(0);
        (
            entered && kept_original && cpus && destination_report && original_blocked,
            began,
            cleaned && original_report,
            gp.try_complete_period(),
        )
    };
    assert_eq!(observed, (true, Some(1), true, Some(1)));
    assert!(!exited.is_reading());
    assert!(!exited.is_enrolled());
    assert_eq!(nodes[0].reader_count(), 0);
    assert_eq!(nodes[1].reader_count(), 0);
}

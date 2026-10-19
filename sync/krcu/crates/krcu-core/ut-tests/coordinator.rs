// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#[cfg(not(test))]
use unittest::{assert, assert_eq};

use super::*;

fn coordinator<const CPUS: usize>(
    nodes: &[ReaderNode; CPUS],
    participants: [bool; CPUS],
) -> GracePeriodCoordinator<'_, CPUS> {
    GracePeriodCoordinator::new(nodes, participants).expect("nonempty participant fixture")
}

#[cfg_attr(test, test)]
#[cfg_attr(not(test), unittest::def_test)]
fn empty_participant_sets_are_rejected() {
    let nodes: [ReaderNode; 2] = core::array::from_fn(|_| ReaderNode::new());
    assert!(GracePeriodCoordinator::new(&nodes, [false, false]).is_none());
    assert!(GracePeriodCoordinator::new(&[], []).is_none());
}

#[cfg_attr(test, test)]
#[cfg_attr(not(test), unittest::def_test)]
fn early_reports_cannot_bypass_the_initialization_gate() {
    let nodes: [ReaderNode; 2] = core::array::from_fn(|_| ReaderNode::new());
    let gp = coordinator(&nodes, [true, true]);
    let observed = {
        let _guard = NoPreemptIrqSave::new();
        // Pause begin_period between its real node transactions without adding
        // a production callback or depending on a race with another CPU.
        let started = gp.start_initialization();
        let pending_before_nodes = gp.state.lock().pending;
        let first_ready = nodes[0].begin_period(1) && gp.observe_cpu(0, 1) && gp.report_node(0);
        let before_second = gp.try_complete_period();
        let uninitialized_report = gp.report_node(1);
        let second_ready = nodes[1].begin_period(1) && gp.observe_cpu(1, 1) && gp.report_node(1);
        let pending_after_reports = gp.state.lock().pending;
        let before_gate = gp.try_complete_period();
        let completed_before_gate = gp.completed_period();
        gp.finish_initialization(1);
        let completed = gp.try_complete_period();
        (
            started,
            pending_before_nodes,
            first_ready && second_ready,
            before_second,
            uninitialized_report,
            pending_after_reports,
            before_gate,
            completed_before_gate,
            completed,
        )
    };
    assert_eq!(
        observed,
        (
            Some(1),
            [true, true],
            true,
            None,
            false,
            [false, false],
            None,
            0,
            Some(1)
        )
    );
    assert_eq!(gp.completed_period(), 1);
}

#[cfg_attr(test, test)]
#[cfg_attr(not(test), unittest::def_test)]
fn partial_finalization_neither_publishes_completion_nor_allows_restart() {
    let nodes: [ReaderNode; 2] = core::array::from_fn(|_| ReaderNode::new());
    let gp = coordinator(&nodes, [true, true]);
    let observed = {
        let _guard = NoPreemptIrqSave::new();
        let started = gp.begin_period();
        let ready =
            gp.observe_cpu(0, 1) && gp.report_node(0) && gp.observe_cpu(1, 1) && gp.report_node(1);
        let finalizing = gp.start_finalization();
        let first_finished = nodes[0].finish_period(1);
        let completed_early = gp.completed_period();
        let overlapping_start = gp.begin_period();
        let overlapping_finish = gp.try_complete_period();
        let late_report = gp.report_node(1);
        let second_finished = nodes[1].finish_period(1);
        gp.finish_finalization(1);
        (
            started,
            ready && first_finished && second_finished,
            finalizing,
            completed_early,
            overlapping_start,
            overlapping_finish,
            late_report,
            gp.completed_period(),
            gp.begin_period(),
        )
    };
    assert_eq!(
        observed,
        (Some(1), true, Some(1), 0, None, None, false, 1, Some(2))
    );
}

#[cfg_attr(test, test)]
#[cfg_attr(not(test), unittest::def_test)]
fn only_admitted_nodes_owe_reports_and_duplicates_do_not_clear_peers() {
    let nodes: [ReaderNode; 3] = core::array::from_fn(|_| ReaderNode::new());
    let gp = coordinator(&nodes, [true, false, true]);
    let observed = {
        let _guard = NoPreemptIrqSave::new();
        let started = gp.begin_period();
        let busy = gp.begin_period();
        let premature_report = gp.report_node(0);
        let inactive = gp.observe_cpu(1, 1) || gp.report_node(1);
        let invalid = gp.observe_cpu(3, 1) || gp.report_node(3);
        let first = gp.observe_cpu(0, 1) && gp.report_node(0);
        let duplicate = gp.report_node(0);
        let before_peer = gp.try_complete_period();
        let peer = gp.observe_cpu(2, 1) && gp.report_node(2);
        let complete = gp.try_complete_period();
        let repeated_complete = gp.try_complete_period();
        (
            started,
            busy,
            premature_report,
            inactive || invalid,
            first && peer,
            duplicate,
            before_peer,
            complete,
            repeated_complete,
        )
    };
    assert_eq!(
        observed,
        (
            Some(1),
            None,
            false,
            false,
            true,
            false,
            None,
            Some(1),
            None
        )
    );
    assert_eq!(nodes[0].ready_period(), None);
    assert_eq!(nodes[2].ready_period(), None);
    // The unused capacity slot was never initialized by this coordinator.
    assert!(nodes[1].begin_period(8));
}

#[cfg_attr(test, test)]
#[cfg_attr(not(test), unittest::def_test)]
fn nearby_stale_reports_cannot_complete_the_next_period_across_wrap() {
    let nodes: [ReaderNode; 2] = core::array::from_fn(|_| ReaderNode::new());
    let gp = coordinator(&nodes, [true, true]);
    let observed = {
        let _guard = NoPreemptIrqSave::new();
        gp.state.lock().completed = PeriodId::MAX - 1;
        let first = gp.begin_period();
        let first_ready = gp.observe_cpu(0, PeriodId::MAX)
            && gp.report_node(0)
            && gp.observe_cpu(1, PeriodId::MAX)
            && gp.report_node(1);
        let first_complete = gp.try_complete_period();
        let next = gp.begin_period();
        let next_peer = gp.observe_cpu(1, 0) && gp.report_node(1);
        // Direct acceptance models a bounded stale capture from the preceding
        // period. It does not assert safety for a report held for a full wrap.
        let stale = gp.accept_report(0, PeriodId::MAX);
        let incomplete = gp.try_complete_period();
        let current = gp.observe_cpu(0, 0) && gp.report_node(0);
        let complete = gp.try_complete_period();
        (
            first,
            first_ready,
            first_complete,
            next,
            next_peer,
            stale,
            incomplete,
            current,
            complete,
        )
    };
    assert_eq!(
        observed,
        (
            Some(PeriodId::MAX),
            true,
            Some(PeriodId::MAX),
            Some(0),
            true,
            false,
            None,
            true,
            Some(0)
        )
    );
}

#[cfg_attr(test, test)]
#[cfg_attr(not(test), unittest::def_test)]
fn cpu_report_cannot_discharge_an_enrolled_old_reader() {
    let nodes: [ReaderNode; 2] = core::array::from_fn(|_| ReaderNode::new());
    let gp = coordinator(&nodes, [true, true]);
    let reader = crate::TaskReader::new();
    let observed = {
        let _guard = NoPreemptIrqSave::new();
        let during = reader.with_read(|_| {
            // SAFETY: exclusive stable fixture under scheduler exclusion; the
            // scope unlinks the reader before it or either node can be dropped.
            unsafe { nodes[0].enroll(&reader) };
            let started = gp.begin_period();
            let cpus = gp.observe_cpu(0, 1) && gp.observe_cpu(1, 1);
            let blocked = gp.report_node(0);
            let peer = gp.report_node(1);
            (started, cpus, blocked, peer, gp.try_complete_period())
        });
        let after_exit = gp.report_node(0);
        (during, after_exit, gp.try_complete_period())
    };
    assert_eq!(
        observed,
        ((Some(1), true, false, true, None), true, Some(1))
    );
    assert!(!reader.is_enrolled());
}

#[cfg_attr(test, test)]
#[cfg_attr(not(test), unittest::def_test)]
fn delayed_node_inspection_uses_current_obligations_after_multiple_wraps() {
    let nodes: [ReaderNode; 2] = core::array::from_fn(|_| ReaderNode::new());
    let gp = coordinator(&nodes, [true, true]);
    // Deferred work retains only the node index, never a captured ready ID.
    let deferred_node = 0;
    let modulus = usize::from(PeriodId::MAX) + 1;
    let mut expected: PeriodId = 0;
    for _ in 0..(2 * modulus + 1) {
        expected = expected.wrapping_add(1);
        let observed = {
            let _guard = NoPreemptIrqSave::new();
            let started = gp.begin_period();
            let peer = gp.observe_cpu(1, expected) && gp.report_node(1);
            let deferred_before_observation = gp.report_node(deferred_node);
            let premature_completion = gp.try_complete_period();
            let observed_cpu = gp.observe_cpu(deferred_node, expected);
            let deferred_after_observation = gp.report_node(deferred_node);
            let duplicate = gp.report_node(deferred_node);
            (
                started,
                peer,
                deferred_before_observation,
                premature_completion,
                observed_cpu && deferred_after_observation,
                duplicate,
                gp.try_complete_period(),
            )
        };
        assert_eq!(
            observed,
            (
                Some(expected),
                true,
                false,
                None,
                true,
                false,
                Some(expected)
            )
        );
        assert_eq!(gp.completed_period(), expected);
    }
    assert_eq!(expected, 1);
}

#[cfg_attr(test, test)]
#[cfg_attr(not(test), unittest::def_test)]
fn pending_reporter_cpu_bounds_capture_to_the_following_period() {
    let nodes: [ReaderNode; 2] = core::array::from_fn(|_| ReaderNode::new());
    let gp = coordinator(&nodes, [true, true]);
    let observed = {
        let _guard = NoPreemptIrqSave::new();
        gp.state.lock().completed = PeriodId::MAX - 1;
        let began = gp.begin_period();
        let cpus = gp.observe_cpu(0, PeriodId::MAX) && gp.observe_cpu(1, PeriodId::MAX);
        let captured = nodes[0].ready_period();
        // Another reporter completes the same period while the first capture
        // is in flight. CPU 1 represents that capture's watched execution CPU.
        let reports = gp.report_node(0) && gp.report_node(1);
        let first_complete = gp.try_complete_period();
        let next = gp.begin_period();
        // Submit while node 0 still owes a report, so rejection must check
        // identity rather than merely seeing an already-cleared pending bit.
        let stale = captured.is_some_and(|period| gp.accept_report(0, period));
        let next_peer = gp.observe_cpu(0, 0) && gp.report_node(0);
        // Host integration must not report CPU 1 quiescent during its watched
        // interval. With that obligation retained, no further period can finish.
        let blocked = gp.try_complete_period();
        let cannot_restart = gp.begin_period();
        let still_blocked = gp.try_complete_period();
        // The captured report is now consumed. Model the host's next valid
        // boundary explicitly, rather than inferring one from this test's guard.
        let resumed = gp.observe_cpu(1, 0) && gp.report_node(1);
        (
            began == Some(PeriodId::MAX) && cpus && reports,
            captured,
            first_complete,
            next,
            next_peer && resumed,
            (blocked, cannot_restart, stale, still_blocked),
            gp.try_complete_period(),
        )
    };
    assert_eq!(
        observed,
        (
            true,
            Some(PeriodId::MAX),
            Some(PeriodId::MAX),
            Some(0),
            true,
            (None, None, false, None),
            Some(0)
        )
    );
}

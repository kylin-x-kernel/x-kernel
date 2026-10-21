// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq};

use super::*;

#[unittest::def_test]
fn deferred_outer_exit_never_relocks_its_owner_node() {
    let node = ReaderNode::new();
    let reader = TaskReader::new();
    let _guard = kspin::NoPreemptIrqSave::new();
    let enroll_fn = |_: &crate::ReadGuard<'_>| {
        // SAFETY: exclusive active fixture under scheduler exclusion.
        unsafe { node.enroll(&reader) };
    };
    // SAFETY: isolated fixture, stable until explicit cleanup; no concurrent
    // scope execution. The callback models enrollment before interruption.
    unsafe { reader.with_read_deferred(enroll_fn) };
    let state = node.state.lock();
    let interrupt_fn = |_: &crate::ReadGuard<'_>| {
        // SAFETY: balanced inner interrupt of the same isolated reader.
        unsafe { reader.with_read_deferred(|_| reader.is_reading()) }
    };
    // SAFETY: strictly nested synthetic interrupt scopes only change nesting;
    // neither callback touches enrollment or calls ordinary scope cleanup.
    let nested = unsafe { reader.with_read_deferred(interrupt_fn) };
    let pending = !reader.is_reading() && reader.is_enrolled();
    drop(state);
    // SAFETY: no node lock held, inactive stable fixture under exclusion.
    let cleaned = unsafe { reader.finish_deferred_cleanup() };
    assert!(nested);
    assert!(pending);
    assert!(cleaned);
    assert_eq!(node.reader_count(), 0);
}

#[unittest::def_test]
fn scheduler_reports_once_per_cpu_obligation_including_wrap() {
    let node = ReaderNode::new();
    let reader = TaskReader::new();
    let _guard = kspin::NoPreemptIrqSave::new();
    // SAFETY: stable isolated scheduler fixture under exclusion, no scopes.
    assert_eq!(unsafe { node.note_context_switch(&reader) }, None);
    for period in [PeriodId::MAX, 0, 1] {
        assert!(node.begin_period(period));
        assert!(node.needs_cpu_observation());
        assert!(!node.observe_cpu(period.wrapping_sub(1)));
        assert!(node.needs_cpu_observation());
        // SAFETY: this fixture exclusively supplies its scheduler boundaries.
        assert_eq!(unsafe { node.note_context_switch(&reader) }, Some(period));
        assert!(!node.needs_cpu_observation());
        // SAFETY: serial repeated boundary of the same retained fixture.
        assert_eq!(unsafe { node.note_context_switch(&reader) }, None);
        assert_eq!(node.ready_period(), Some(period));
        assert!(node.finish_period(period));
        assert!(!node.needs_cpu_observation());
    }
}

#[unittest::def_test]
fn observed_cpu_still_enrolls_readers_for_the_next_period() {
    let node = ReaderNode::new();
    let inactive = TaskReader::new();
    let reader = TaskReader::new();
    let _guard = kspin::NoPreemptIrqSave::new();
    assert!(node.begin_period(1));
    // SAFETY: isolated stable outgoing-task fixture under exclusion.
    assert_eq!(unsafe { node.note_context_switch(&inactive) }, Some(1));
    let observed = reader.with_read(|_| {
        // SAFETY: exclusive outgoing reader; storage outlives its scope exit.
        let reported = unsafe { node.note_context_switch(&reader) };
        let enrolled = reader.is_enrolled_on(&node);
        let finished = node.finish_period(1);
        let started = node.begin_period(2);
        // SAFETY: exclusive subsequent scheduler boundary; reader stays blocked.
        let next_report = unsafe { node.note_context_switch(&inactive) };
        (
            (reported, enrolled, finished, started),
            (
                next_report,
                node.needs_cpu_observation(),
                node.ready_period(),
                node.finish_period(2),
            ),
        )
    });
    assert_eq!(
        observed,
        ((None, true, true, true), (None, false, None, false))
    );
    // CPU observation was already supplied. Reader exit makes the node ready
    // for the GP scan without requiring another scheduler report.
    assert_eq!(node.ready_period(), Some(2));
    // SAFETY: isolated stable inactive outgoing task under scheduler exclusion.
    assert_eq!(unsafe { node.note_context_switch(&inactive) }, None);
    assert!(node.finish_period(2));
}

#[unittest::def_test]
fn idle_scheduler_path_preserves_startup_readers() {
    let node = ReaderNode::new();
    let reader = TaskReader::new();
    let _guard = kspin::NoPreemptIrqSave::new();
    let observed = reader.with_read(|_| {
        let was_pending = node.needs_cpu_observation();
        // SAFETY: exclusive stable outgoing reader; no period exists yet.
        let reported = unsafe { node.note_context_switch(&reader) };
        let enrolled = reader.is_enrolled_on(&node);
        let started = node.begin_period(1);
        // SAFETY: serial scheduler boundary while the enrolled scope is active.
        let next_report = unsafe { node.note_context_switch(&reader) };
        (
            (was_pending, reported, enrolled, started),
            (
                next_report,
                node.needs_cpu_observation(),
                node.ready_period(),
            ),
        )
    });
    assert_eq!(observed, ((false, None, true, true), (None, false, None)));
    assert_eq!(node.ready_period(), Some(1));
    assert!(node.finish_period(1));
}

#[unittest::def_test]
fn repeated_enrollment_keeps_original_owner_until_outer_exit() {
    let first = ReaderNode::new();
    let second = ReaderNode::new();
    let reader = TaskReader::new();
    let observed = {
        let _guard = kspin::NoPreemptIrqSave::new();
        reader.with_read(|_| {
            // SAFETY: this isolated scheduler fixture is exclusively driven
            // here with IRQs/preemption excluded (no-op on host). No actual
            // task references it; stack-stable storage outlives every removal.
            unsafe { first.enroll(&reader) };
            reader.with_read(|_| {
                // SAFETY: serial duplicate call, with the same exclusion and
                // stable fixture lifetime; membership stays on the first node.
                unsafe { second.enroll(&reader) };
                // SAFETY: same exclusive fixture and lifetime as above.
                unsafe { first.enroll(&reader) };
            });
            (
                reader.is_enrolled_on(&first),
                first.reader_count(),
                second.reader_count(),
            )
        })
    };
    assert_eq!(observed, (true, 1, 0));
    assert!(!reader.is_enrolled());
    assert_eq!(first.reader_count(), 0);
}

#[unittest::def_test]
fn removal_preserves_other_readers_and_inactive_tasks_do_not_enroll() {
    let node = ReaderNode::new();
    let first = TaskReader::new();
    let second = TaskReader::new();
    let (inactive_count, both_count, after_inner) = {
        let _guard = kspin::NoPreemptIrqSave::new();
        // SAFETY: isolated scheduler fixture, no task or other CPU accesses it;
        // IRQs/preemption excluded, all storage stable through scope removal.
        unsafe { node.enroll(&first) };
        let inactive_count = node.reader_count();
        let (both_count, after_inner) = first.with_read(|_| {
            // SAFETY: same exclusive fixture; first's scope ends before storage.
            unsafe { node.enroll(&first) };
            let both_count = second.with_read(|_| {
                // SAFETY: second has distinct links and the same stable lifetime.
                unsafe { node.enroll(&second) };
                node.reader_count()
            });
            (
                both_count,
                (
                    first.is_enrolled_on(&node),
                    second.is_enrolled(),
                    node.reader_count(),
                ),
            )
        });
        (inactive_count, both_count, after_inner)
    };
    assert_eq!(inactive_count, 0);
    assert_eq!(both_count, 2);
    assert_eq!(after_inner, (true, false, 1));
    assert_eq!(node.reader_count(), 0);
}

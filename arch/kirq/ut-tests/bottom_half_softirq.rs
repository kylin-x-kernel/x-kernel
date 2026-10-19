// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::sync::atomic::{AtomicUsize, Ordering};

use unittest::{assert, assert_eq, def_test};

use super::{
    SoftirqDiagnostics, SoftirqRunResult, SoftirqVec, clear_softirq_diagnostics,
    local_softirq_pending, raise_softirq, raise_softirq_irqoff, run_pending_softirqs,
    softirq_diagnostics,
    test_support::{ScopedSoftirqAction, begin_softirq_test},
};
use crate::context::{
    HardIrqContextGuard, SoftIrqContextGuard, clear_irq_context_diagnostics,
    irq_context_diagnostics, irq_context_snapshot, local_bh_disable,
};

static ORDER_SEQ: AtomicUsize = AtomicUsize::new(1);
static HIGH_ORDER: AtomicUsize = AtomicUsize::new(0);
static TIMER_ORDER: AtomicUsize = AtomicUsize::new(0);
static RERAISE_CALLS: AtomicUsize = AtomicUsize::new(0);
static LIMIT_CALLS: AtomicUsize = AtomicUsize::new(0);
static ACTION_IRQ_ENABLED: AtomicUsize = AtomicUsize::new(0);

fn run_with_resched(needs_resched: impl Fn() -> bool) -> SoftirqRunResult {
    let _guard = kspin::NoPreemptIrqSave::new();
    super::run_pending_softirqs_irqoff_with_resched(needs_resched)
}

fn record_order(slot: &AtomicUsize) {
    if slot.load(Ordering::Relaxed) == 0 {
        let order = ORDER_SEQ.fetch_add(1, Ordering::Relaxed);
        // A competing callback may have published the first observation; keep it.
        let _ = slot.compare_exchange(0, order, Ordering::Relaxed, Ordering::Relaxed);
    }
}

fn high_action() {
    record_order(&HIGH_ORDER);
}

/// Group registration either installs both entries or preserves every slot.
///
/// # Panics
/// Panics on partial publication, replacement or missing installed actions.
#[def_test(serial)]
fn test_softirq_group_registration_is_all_or_nothing() {
    let _wake_gate = begin_softirq_test();
    let _first = ScopedSoftirqAction::clear(SoftirqVec::IrqPoll);
    // Hrtimer has no production owner; Rcu belongs to the live GP service.
    let _second = ScopedSoftirqAction::install(SoftirqVec::Hrtimer, timer_action);
    let entries: [(SoftirqVec, super::SoftirqAction); 2] = [
        (SoftirqVec::IrqPoll, high_action),
        (SoftirqVec::Hrtimer, high_action),
    ];
    assert!(!super::open_softirqs(&entries));
    assert!(!super::is_softirq_open(SoftirqVec::IrqPoll));
    assert!(super::softirq_action_matches_for_tests(
        SoftirqVec::Hrtimer,
        timer_action
    ));
    {
        let _empty = ScopedSoftirqAction::clear(SoftirqVec::Hrtimer);
        assert!(super::open_softirqs(&entries));
        for (vec, action) in entries {
            assert!(super::softirq_action_matches_for_tests(vec, action));
        }
        assert!(!super::open_softirqs(&entries));
    }
}

/// Duplicate and oversized groups reject unchanged; empty input is a no-op.
///
/// # Panics
/// Panics if invalid input installs an action or empty input fails.
#[def_test(serial)]
fn test_softirq_group_registration_validates_before_publication() {
    let _wake_gate = begin_softirq_test();
    let _empty = ScopedSoftirqAction::clear(SoftirqVec::IrqPoll);
    assert!(super::open_softirqs(&[]));
    let entry = (SoftirqVec::IrqPoll, high_action as super::SoftirqAction);
    assert!(!super::open_softirqs(&[entry, entry]));
    assert!(!super::is_softirq_open(SoftirqVec::IrqPoll));
    assert!(!super::open_softirqs(&[entry; SoftirqVec::COUNT + 1]));
    assert!(!super::is_softirq_open(SoftirqVec::IrqPoll));
}

fn timer_action() {
    record_order(&TIMER_ORDER);
}

fn record_local_irq_state_action() {
    ACTION_IRQ_ENABLED.store(karch::local_irq_enabled() as usize, Ordering::Relaxed);
}

fn reraise_action() {
    let calls = RERAISE_CALLS.fetch_add(1, Ordering::Relaxed);
    if calls == 0 {
        raise_softirq(SoftirqVec::Hrtimer);
    }
}

fn always_reraise_action() {
    LIMIT_CALLS.fetch_add(1, Ordering::Relaxed);
    raise_softirq(SoftirqVec::Sched);
}

fn leak_softirq_context_action() {
    let guard = SoftIrqContextGuard::enter();
    core::mem::forget(guard);
}

#[def_test(serial)]
fn test_softirq_runs_in_vector_order() {
    // Publish the complete batch before any IRQ-tail runner can take it.
    let _irq = kspin::NoPreemptIrqSave::new();
    let _wake_gate = begin_softirq_test();
    ORDER_SEQ.store(1, Ordering::Relaxed);
    HIGH_ORDER.store(0, Ordering::Relaxed);
    TIMER_ORDER.store(0, Ordering::Relaxed);
    let _high = ScopedSoftirqAction::install(SoftirqVec::High, high_action);
    let _timer = ScopedSoftirqAction::install(SoftirqVec::Timer, timer_action);

    raise_softirq(SoftirqVec::Timer);
    raise_softirq(SoftirqVec::High);

    assert_eq!(run_pending_softirqs(), SoftirqRunResult::Ran);
    assert!(HIGH_ORDER.load(Ordering::Relaxed) < TIMER_ORDER.load(Ordering::Relaxed));
}

#[def_test(serial)]
fn test_local_bh_guard_defers_until_outer_drop() {
    let _wake_gate = begin_softirq_test();
    HIGH_ORDER.store(0, Ordering::Relaxed);
    let _high = ScopedSoftirqAction::install(SoftirqVec::High, high_action);

    {
        let _outer = local_bh_disable();
        {
            let _inner = local_bh_disable();
            raise_softirq(SoftirqVec::High);
            assert_eq!(HIGH_ORDER.load(Ordering::Relaxed), 0);
        }
        assert_eq!(HIGH_ORDER.load(Ordering::Relaxed), 0);
    }

    assert!(HIGH_ORDER.load(Ordering::Relaxed) != 0);
    assert_eq!(local_softirq_pending(), 0);
}

#[def_test(serial)]
fn test_task_context_raise_requests_softirq_daemon_wake() {
    // Keep the publication and exact wake counters in one local test window.
    let _irq = kspin::NoPreemptIrqSave::new();
    let _wake_gate = begin_softirq_test();
    let _timer = ScopedSoftirqAction::clear(SoftirqVec::Timer);

    raise_softirq(SoftirqVec::Timer);

    let diagnostics = softirq_diagnostics();
    assert_eq!(diagnostics.daemon_wake_requests, 1);
    assert_eq!(diagnostics.daemon_wake_suppressed_context, 0);
}

#[def_test(serial)]
fn test_softirq_action_runs_with_local_irq_enabled() {
    // Exclude automatic drains before our explicit pass. The dispatcher
    // itself must enable IRQs inside the action despite this outer guard.
    let _irq = kspin::NoPreemptIrqSave::new();
    let _wake_gate = begin_softirq_test();
    ACTION_IRQ_ENABLED.store(0, Ordering::Relaxed);
    let _block = ScopedSoftirqAction::install(SoftirqVec::Block, record_local_irq_state_action);

    raise_softirq(SoftirqVec::Block);

    assert_eq!(run_pending_softirqs(), SoftirqRunResult::Ran);
    assert_eq!(ACTION_IRQ_ENABLED.load(Ordering::Relaxed), 1);
    assert_eq!(local_softirq_pending(), 0);
}

#[def_test(serial)]
fn test_softirq_reraise_is_not_lost() {
    // The daemon gate alone does not stop real IRQ tails from consuming
    // pending work before the explicit pass. Keep setup and inspection pinned
    // and IRQ-masked; the dispatcher still enables IRQs for each action.
    let _irq = kspin::NoPreemptIrqSave::new();
    let _wake_gate = begin_softirq_test();
    RERAISE_CALLS.store(0, Ordering::Relaxed);
    let _hrtimer = ScopedSoftirqAction::install(SoftirqVec::Hrtimer, reraise_action);

    raise_softirq(SoftirqVec::Hrtimer);
    assert_eq!(run_with_resched(|| false), SoftirqRunResult::Ran);
    assert_eq!(RERAISE_CALLS.load(Ordering::Relaxed), 2);
}

#[def_test(serial)]
fn test_softirq_restart_limit_preserves_pending() {
    // Prevent another IRQ-tail pass between our bounded run and inspection
    // of its retained pending bit and exact restart/wake counters.
    let _irq = kspin::NoPreemptIrqSave::new();
    let _wake_gate = begin_softirq_test();
    LIMIT_CALLS.store(0, Ordering::Relaxed);
    let _sched = ScopedSoftirqAction::install(SoftirqVec::Sched, always_reraise_action);

    raise_softirq(SoftirqVec::Sched);
    clear_softirq_diagnostics();

    assert_eq!(run_with_resched(|| false), SoftirqRunResult::Deferred);
    assert!(LIMIT_CALLS.load(Ordering::Relaxed) >= 10);
    assert!(local_softirq_pending() & (1usize << SoftirqVec::Sched.as_usize()) != 0);
    assert_eq!(softirq_diagnostics().restart_limit_hits, 1);
    assert_eq!(softirq_diagnostics().daemon_wake_requests, 1);
}

#[def_test(serial)]
fn test_softirq_resched_finishes_batch_and_preserves_pending() {
    // Retained pending belongs to the next explicit pass, not an IRQ tail.
    let _irq = kspin::NoPreemptIrqSave::new();
    let _wake_gate = begin_softirq_test();
    RERAISE_CALLS.store(0, Ordering::Relaxed);
    HIGH_ORDER.store(0, Ordering::Relaxed);
    let _high = ScopedSoftirqAction::install(SoftirqVec::High, high_action);
    let _hrtimer = ScopedSoftirqAction::install(SoftirqVec::Hrtimer, reraise_action);
    raise_softirq(SoftirqVec::High);
    raise_softirq(SoftirqVec::Hrtimer);
    clear_softirq_diagnostics();

    assert_eq!(run_with_resched(|| true), SoftirqRunResult::Deferred);
    assert!(HIGH_ORDER.load(Ordering::Relaxed) != 0);
    assert_eq!(RERAISE_CALLS.load(Ordering::Relaxed), 1);
    assert!(local_softirq_pending() & (1usize << SoftirqVec::Hrtimer.as_usize()) != 0);
    assert_eq!(softirq_diagnostics().daemon_wake_requests, 1);
    assert_eq!(softirq_diagnostics().restart_limit_hits, 0);

    assert_eq!(run_with_resched(|| false), SoftirqRunResult::Ran);
    assert_eq!(RERAISE_CALLS.load(Ordering::Relaxed), 2);
    assert_eq!(local_softirq_pending(), 0);
}

#[def_test(serial)]
fn test_softirq_empty_pending_does_not_query_resched() {
    // Otherwise an automatic drain could bypass the injected resched query.
    let _irq = kspin::NoPreemptIrqSave::new();
    let _wake_gate = begin_softirq_test();
    let _high = ScopedSoftirqAction::install(SoftirqVec::High, high_action);
    raise_softirq(SoftirqVec::High);
    clear_softirq_diagnostics();

    assert_eq!(
        run_with_resched(|| panic!("drained work must not query rescheduling")),
        SoftirqRunResult::Ran
    );
    assert_eq!(softirq_diagnostics().daemon_wake_requests, 0);
    assert_eq!(local_softirq_pending(), 0);
}

#[def_test(serial)]
fn test_softirq_deferred_in_bh_disabled_context() {
    // Real IRQ tails also record BH-disabled deferrals on this CPU.
    // Exclude them while checking the exact synthetic-run counters.
    let _irq_guard = kspin::NoPreemptIrqSave::new();
    let _wake_gate = begin_softirq_test();
    HIGH_ORDER.store(0, Ordering::Relaxed);
    let _high = ScopedSoftirqAction::install(SoftirqVec::High, high_action);
    let guard = local_bh_disable();
    raise_softirq(SoftirqVec::High);

    assert_eq!(run_pending_softirqs(), SoftirqRunResult::Deferred);
    assert_eq!(softirq_diagnostics().context_deferred_runs, 1);
    assert_eq!(softirq_diagnostics().daemon_wake_requests, 0);
    assert_eq!(softirq_diagnostics().daemon_wake_suppressed_context, 1);
    assert_eq!(HIGH_ORDER.load(Ordering::Relaxed), 0);
    drop(guard);
    assert!(HIGH_ORDER.load(Ordering::Relaxed) != 0);
}

#[def_test(serial)]
fn test_softirq_context_restored_after_unhandled_vector() {
    let _wake_gate = begin_softirq_test();
    let _irq_poll = ScopedSoftirqAction::clear(SoftirqVec::IrqPoll);
    let before = irq_context_snapshot();
    raise_softirq(SoftirqVec::IrqPoll);
    let _ = run_pending_softirqs();
    assert_eq!(irq_context_snapshot(), before);
    let diagnostics = softirq_diagnostics();
    assert!(diagnostics.unhandled_vector_warnings >= 1);
}

#[def_test(serial)]
fn test_softirq_context_restored_after_handler_leak() {
    // Attribute the deliberate leak to our explicit pass and inspect it
    // before an IRQ tail or task migration can change the observed state.
    let _irq = kspin::NoPreemptIrqSave::new();
    let _wake_gate = begin_softirq_test();
    let _tasklet = ScopedSoftirqAction::install(SoftirqVec::Tasklet, leak_softirq_context_action);
    let before = irq_context_snapshot();

    raise_softirq(SoftirqVec::Tasklet);
    assert_eq!(run_pending_softirqs(), SoftirqRunResult::Ran);

    assert_eq!(irq_context_snapshot(), before);
    assert_eq!(softirq_diagnostics().context_leak_warnings, 1);
}

#[def_test(serial)]
fn test_softirq_deferred_in_hardirq_context() {
    // A synthetic context guard does not mask hardware IRQs. A real IRQ
    // returning inside that scope would add another deferred-run observation.
    // Restore actions and synthetic context before restoring local IRQs.
    let _irq = kspin::NoPreemptIrqSave::new();
    let _wake_gate = begin_softirq_test();
    clear_irq_context_diagnostics();
    HIGH_ORDER.store(0, Ordering::Relaxed);
    let _high = ScopedSoftirqAction::install(SoftirqVec::High, high_action);

    {
        let _hardirq = HardIrqContextGuard::enter();
        let bh_guard = local_bh_disable();
        raise_softirq(SoftirqVec::High);

        assert_eq!(run_pending_softirqs(), SoftirqRunResult::Deferred);
        drop(bh_guard);
        assert_eq!(HIGH_ORDER.load(Ordering::Relaxed), 0);
        assert_eq!(irq_context_diagnostics().bh_in_hardirq_warnings, 1);
        assert_eq!(softirq_diagnostics().context_deferred_runs, 1);
        assert_eq!(softirq_diagnostics().daemon_wake_suppressed_context, 1);
    }

    assert_eq!(run_pending_softirqs(), SoftirqRunResult::Ran);
    assert!(HIGH_ORDER.load(Ordering::Relaxed) != 0);
}

#[def_test(serial)]
fn test_repeated_task_context_raise_requests_single_daemon_wake() {
    // An intervening drain would legitimately make the second raise wake again.
    let _irq = kspin::NoPreemptIrqSave::new();
    let _wake_gate = begin_softirq_test();
    let _timer = ScopedSoftirqAction::clear(SoftirqVec::Timer);
    let _high = ScopedSoftirqAction::clear(SoftirqVec::High);

    raise_softirq(SoftirqVec::Timer);
    raise_softirq(SoftirqVec::High);

    assert_eq!(softirq_diagnostics().daemon_wake_requests, 1);
    assert_eq!(softirq_diagnostics().daemon_wake_suppressed_context, 0);
}

#[def_test(serial)]
fn test_raise_softirq_irqoff_reports_enabled_irq_misuse() {
    let _wake_gate = begin_softirq_test();
    // Keep IRQs enabled to exercise misuse, but prevent IRQ-tail consumption
    // until the action guard clears our pending bit before BH is re-enabled.
    let _bh = local_bh_disable();
    let _timer = ScopedSoftirqAction::clear(SoftirqVec::Timer);

    assert!(karch::local_irq_enabled());

    raise_softirq_irqoff(SoftirqVec::Timer);

    assert_eq!(softirq_diagnostics().irqoff_misuse_warnings, 1);
    assert!(karch::local_irq_enabled());
    assert!(local_softirq_pending() & (1usize << SoftirqVec::Timer.as_usize()) != 0);
}

#[def_test(serial)]
fn test_softirq_diagnostics_clear() {
    // Do not admit local IRQ-tail observations between clearing and reading.
    let _irq = kspin::NoPreemptIrqSave::new();
    let _wake_gate = begin_softirq_test();
    let _ = softirq_diagnostics();
    clear_softirq_diagnostics();
    assert_eq!(softirq_diagnostics(), SoftirqDiagnostics::default());
}

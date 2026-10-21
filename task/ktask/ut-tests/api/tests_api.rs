// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use kirq::{
    context::local_bh_disable,
    softirq::{
        SoftirqRunResult, SoftirqVec, raise_softirq,
        test_support::{ScopedSoftirqAction, begin_softirq_test},
    },
};
use unittest::{assert, assert_eq, def_test};

use super::{
    MIN_SCHED_TIMER_NS, can_sleep_current_context_for_tests, current, sched_stats_text,
    schedule_deadline_request, select_local_timer_deadline, wait_for_poll_event_until,
    with_wake_sync,
};

static SOFTIRQ_SLEEPABLE_OBSERVED: AtomicBool = AtomicBool::new(true);
static SOFTIRQ_IRQS_ENABLED_OBSERVED: AtomicBool = AtomicBool::new(false);
static SOFTIRQ_SERVING_OBSERVED: AtomicBool = AtomicBool::new(false);

fn record_sleep_gate_from_softirq() {
    SOFTIRQ_IRQS_ENABLED_OBSERVED.store(karch::local_irq_enabled(), Ordering::Release);
    SOFTIRQ_SERVING_OBSERVED.store(kirq::context::is_serving_softirq(), Ordering::Release);
    SOFTIRQ_SLEEPABLE_OBSERVED.store(can_sleep_current_context_for_tests(), Ordering::Release);
}

#[def_test]
fn timer_arbiter_picks_earliest_deadline() {
    assert_eq!(select_local_timer_deadline(Some(30), Some(10)), Some(10));
    assert_eq!(select_local_timer_deadline(Some(5), Some(8)), Some(5));
    assert_eq!(select_local_timer_deadline(None, Some(7)), Some(7));
}

#[def_test]
fn timer_arbiter_disarms_when_all_empty() {
    assert_eq!(select_local_timer_deadline(None, None), None);
}

#[def_test]
fn timer_arbiter_merges_prepared_without_losing_other_sources() {
    assert_eq!(select_local_timer_deadline(Some(30), Some(10)), Some(10));
    assert_eq!(select_local_timer_deadline(Some(5), Some(10)), Some(5));
    assert_eq!(select_local_timer_deadline(None, Some(0)), Some(0));
    assert_eq!(
        select_local_timer_deadline(None, Some(u64::MAX)),
        Some(u64::MAX)
    );
}

#[def_test]
fn timer_arbiter_keeps_sched_after_owned_timer_delivery() {
    // Delivery at T=10 must not clear a later schedule deadline at T=50.
    assert_eq!(select_local_timer_deadline(Some(50), Some(10)), Some(10));
    assert_eq!(select_local_timer_deadline(Some(50), None), Some(50));
}

#[def_test]
fn timer_arbiter_past_deadline_still_selected() {
    // Expired deadlines remain the earliest absolute value; the IRQ path
    // re-accounts and reprograms rather than skipping them.
    assert_eq!(select_local_timer_deadline(Some(1), Some(150)), Some(1));
}

#[def_test]
fn immediate_schedule_request_sets_pending_without_rearming_hrtick() {
    assert_eq!(schedule_deadline_request(100, Some(0)), (None, true));
    assert_eq!(
        schedule_deadline_request(100, Some(1)),
        (Some(100 + MIN_SCHED_TIMER_NS), false)
    );
    assert_eq!(schedule_deadline_request(100, None), (None, false));
}

#[def_test(serial)]
fn with_wake_sync_nested_refcount() {
    // `unittest::assert!` returns from the enclosing fn, so capture flags
    // inside `with_wake_sync` closures instead of asserting there.
    let mut outer_active = false;
    let mut inner_active = false;
    let mut after_inner_still_active = false;

    assert!(!current().is_wake_sync());
    with_wake_sync(|| {
        outer_active = current().is_wake_sync();
        with_wake_sync(|| {
            inner_active = current().is_wake_sync();
        });
        after_inner_still_active = current().is_wake_sync();
    });

    assert!(outer_active);
    assert!(inner_active);
    assert!(after_inner_still_active);
    assert!(!current().is_wake_sync());
}

#[def_test(serial)]
fn wait_for_poll_event_until_blocks_until_condition_and_notify() {
    let event = Arc::new(kpoll::PollEvent::new());
    let started_event = Arc::new(kpoll::PollEvent::new());
    let started = Arc::new(AtomicBool::new(false));
    let ready = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    let waiter_event = event.clone();
    let waiter_started_event = started_event.clone();
    let waiter_started = started.clone();
    let waiter_ready = ready.clone();
    let waiter_finished = finished.clone();

    let waiter = crate::spawn(move || {
        wait_for_poll_event_until(&waiter_event, || {
            let is_ready = waiter_ready.load(Ordering::Acquire);
            if !is_ready {
                waiter_started.store(true, Ordering::Release);
                waiter_started_event.notify();
            }
            is_ready
        })
        .expect("poll wait should register from task context");
        waiter_finished.store(true, Ordering::Release);
    });

    // Observe a false predicate explicitly; yielding cannot prove that a
    // remote waiter has run, much less that it has parked.
    wait_for_poll_event_until(&started_event, || started.load(Ordering::Acquire))
        .expect("waiter should announce its first unsuccessful check");
    let finished_before_notify = finished.load(Ordering::Acquire);

    ready.store(true, Ordering::Release);
    event.notify();
    // Reap before assertions so a failing check cannot leave a live waiter.
    let exit_code = waiter.join();
    assert!(!finished_before_notify);
    assert_eq!(exit_code, 0);
    assert!(finished.load(Ordering::Acquire));
}

#[def_test]
fn sched_stats_text_matches_feature_gates() {
    let text = sched_stats_text();
    #[cfg(feature = "sched_stat")]
    {
        assert!(text.contains("[sched_stat] begin"));
        assert!(text.contains("wakeup_last_cpu="));
        assert!(text.contains("wakeup_idle_sibling="));
        assert!(text.contains("wakeup_fallback="));
        assert!(text.contains("wakeup_rt_rank="));
        assert!(text.contains("wakeup_sync_handoff="));
        assert!(text.contains("idle_pull="));
        assert!(text.contains("timer_irq_sched="));
        assert!(text.contains("timer_irq_stale="));
        assert!(text.contains("[sched_stat] end"));
        assert!(!text.contains("[eevdf_stat]"));
    }
    #[cfg(not(feature = "sched_stat"))]
    {
        assert!(text.contains("disabled"));
    }
}

#[def_test(serial)]
fn sleep_context_gate_rejects_softirq_context() {
    let _wake_gate = begin_softirq_test();
    // Keep IRQ tails and CPU migration out of the raise-to-drain window.
    // The softirq dispatcher itself enables IRQs while invoking the callback.
    let _irqs_off = kspin::NoPreemptIrqSave::new();
    SOFTIRQ_SLEEPABLE_OBSERVED.store(true, Ordering::Release);
    SOFTIRQ_IRQS_ENABLED_OBSERVED.store(false, Ordering::Release);
    SOFTIRQ_SERVING_OBSERVED.store(false, Ordering::Release);
    let _softirq_action =
        ScopedSoftirqAction::install(SoftirqVec::Block, record_sleep_gate_from_softirq);

    raise_softirq(SoftirqVec::Block);
    assert_eq!(kirq::softirq::run_pending_softirqs(), SoftirqRunResult::Ran);

    assert!(SOFTIRQ_IRQS_ENABLED_OBSERVED.load(Ordering::Acquire));
    assert!(SOFTIRQ_SERVING_OBSERVED.load(Ordering::Acquire));
    assert!(!SOFTIRQ_SLEEPABLE_OBSERVED.load(Ordering::Acquire));
}

#[def_test(serial)]
fn sleep_context_gate_rejects_bh_disabled_context() {
    let _bh = local_bh_disable();
    assert!(!can_sleep_current_context_for_tests());
}

#[def_test]
fn sleep_context_gate_accepts_plain_task_context() {
    assert!(can_sleep_current_context_for_tests());
}

#[def_test(serial)]
fn sleep_context_gate_rejects_irqs_disabled_context() {
    let _irqs_off = kspin::NoPreemptIrqSave::new();
    assert!(!can_sleep_current_context_for_tests());
}

/// Service context checks reject IRQ-masked and preemption-protected callers.
///
/// # Panics
/// Panics if a protected context is accepted or the plain task is rejected.
#[def_test(serial)]
fn service_sleep_context_tracks_local_protection() {
    assert!(super::is_sleepable_context());
    {
        let _irqs_off = kspin::NoPreemptIrqSave::new();
        assert!(!super::is_sleepable_context());
    }
    {
        let _no_preempt = kspin::NoPreempt::new();
        assert!(!super::is_sleepable_context());
    }
    assert!(super::is_sleepable_context());
}

/// Idle tasks cannot enter the blocking scheduler even with IRQs enabled.
///
/// # Panics
/// Panics if the shared gate accepts an idle task or rejects the test task.
#[def_test(serial)]
fn sleep_context_gate_rejects_idle_task() {
    assert!(super::is_sleepable_context());
    let idle = crate::TaskInner::new_current_idle("sleep-gate-idle".into()).into_arc();
    assert!(!super::can_sleep_task_context(&idle));
    assert!(super::is_sleepable_context());
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use kalloc_types::KBox;
use kirq::{
    context::local_bh_disable,
    softirq::{
        SoftirqRunResult, SoftirqVec, raise_softirq,
        test_support::{ScopedSoftirqAction, begin_softirq_test},
    },
};
use unittest::{assert, assert_eq, def_test};

use super::{
    MIN_SCHED_TIMER_NS, NEXT_PERIODIC_TICK_NS, NEXT_SCHED_DEADLINE_NS, PERIODIC_TICK_NS,
    can_sleep_current_context_for_tests, consume_periodic_tick, current, refresh_periodic_tick,
    rearm_local_timer, sched_stats_text, schedule_deadline_request, select_local_timer_deadline,
    wait_for_poll_event_until, with_wake_sync,
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
fn periodic_tick_survives_switches_and_stops_during_idle() {
    let observed = {
        let _guard = kspin::NoPreemptIrqSave::new();
        let saved = NEXT_PERIODIC_TICK_NS.load_current();
        NEXT_PERIODIC_TICK_NS.store_current(0);
        refresh_periodic_tick(true, PERIODIC_TICK_NS / 2);
        let first = NEXT_PERIODIC_TICK_NS.load_current();
        refresh_periodic_tick(true, first - 1);
        let after_switch = NEXT_PERIODIC_TICK_NS.load_current();
        let early = consume_periodic_tick(first - 1);
        let due = consume_periodic_tick(first);
        let after_due = NEXT_PERIODIC_TICK_NS.load_current();
        let late = consume_periodic_tick(PERIODIC_TICK_NS * 4 + 1);
        let after_late = NEXT_PERIODIC_TICK_NS.load_current();
        let repeated = consume_periodic_tick(PERIODIC_TICK_NS * 4 + 1);
        refresh_periodic_tick(false, PERIODIC_TICK_NS * 5);
        let stopped = NEXT_PERIODIC_TICK_NS.load_current();
        refresh_periodic_tick(true, PERIODIC_TICK_NS * 5 + 1);
        let restarted = NEXT_PERIODIC_TICK_NS.load_current();
        NEXT_PERIODIC_TICK_NS.store_current(saved);
        (
            first,
            after_switch,
            early,
            due,
            after_due,
            late,
            after_late,
            repeated,
            stopped,
            restarted,
        )
    };
    assert_eq!(observed.0, PERIODIC_TICK_NS);
    assert_eq!(observed.1, observed.0);
    assert!(!observed.2);
    assert!(observed.3);
    assert_eq!(observed.4, PERIODIC_TICK_NS * 2);
    assert!(observed.5);
    assert_eq!(observed.6, PERIODIC_TICK_NS * 5);
    assert!(!observed.7);
    assert_eq!(observed.8, 0);
    assert_eq!(observed.9, PERIODIC_TICK_NS * 6);
}

#[def_test(serial)]
fn real_timer_irqs_preserve_the_periodic_deadline() {
    struct RuntimeProbe {
        periodic_ticks: Arc<AtomicUsize>,
        invalid_context: Arc<AtomicBool>,
    }

    impl crate::UserTaskRuntime for RuntimeProbe {
        fn on_periodic_tick(&self) {
            if karch::local_irq_enabled() || !kirq::context::is_in_hardirq() {
                self.invalid_context.store(true, Ordering::Relaxed);
            }
            self.periodic_ticks.fetch_add(1, Ordering::Release);
        }

        fn as_any(&self) -> &dyn core::any::Any {
            self
        }
    }

    let periodic_ticks = Arc::new(AtomicUsize::new(0));
    let invalid_context = Arc::new(AtomicBool::new(false));
    let checked_early_irq = Arc::new(AtomicBool::new(false));
    let early_irq_was_valid = Arc::new(AtomicBool::new(false));
    let periodic_advanced = Arc::new(AtomicBool::new(false));
    let shared_deadline_consumed = Arc::new(AtomicBool::new(false));
    let shared_deadline_armed = Arc::new(AtomicBool::new(false));
    let runtime_ns = Arc::new(AtomicU64::new(0));
    let worker_periodic_ticks = periodic_ticks.clone();
    let worker_checked = checked_early_irq.clone();
    let worker_valid = early_irq_was_valid.clone();
    let worker_advanced = periodic_advanced.clone();
    let worker_shared = shared_deadline_consumed.clone();
    let shared_armed = shared_deadline_armed.clone();
    let worker_runtime = runtime_ns.clone();
    let task = crate::prepare_task(crate::TaskInner::new_user(
        crate::PreparedTaskEntry::new(move || {
            let delivered = Arc::new(AtomicBool::new(false));
            let fired_ns = Arc::new(AtomicU64::new(0));
            let observed_periodic_ticks = Arc::new(AtomicUsize::new(0));
            let observed_deadline = Arc::new(AtomicU64::new(0));
            let handler_delivered = delivered.clone();
            let handler_fired_ns = fired_ns.clone();
            let handler_periodic_ticks = worker_periodic_ticks.clone();
            let handler_observed_periodic_ticks = observed_periodic_ticks.clone();
            let handler_observed_deadline = observed_deadline.clone();
            let timer = crate::Timer::new(move |_: crate::TimerControl<'_>, _: crate::Expiration| {
                handler_fired_ns.store(khal::time::monotonic_time_nanos(), Ordering::Relaxed);
                handler_observed_periodic_ticks.store(handler_periodic_ticks.load(Ordering::Acquire), Ordering::Relaxed);
                handler_observed_deadline.store(NEXT_PERIODIC_TICK_NS.load_current(), Ordering::Relaxed);
                handler_delivered.store(true, Ordering::Release);
            });
            let _pin = kspin::NoPreempt::new();
            let limit_ns = khal::time::monotonic_time_nanos().saturating_add(3_000_000_000);
            while worker_periodic_ticks.load(Ordering::Acquire) == 0
                && khal::time::monotonic_time_nanos() < limit_ns
            {
                core::hint::spin_loop();
            }

            let mut original_deadline_ns = 0;
            while khal::time::monotonic_time_nanos() < limit_ns {
                let (before_periodic, deadline_ns, started) = {
                    let _guard = kspin::NoPreemptIrqSave::new();
                    let now_ns = khal::time::monotonic_time_nanos();
                    let deadline_ns = NEXT_PERIODIC_TICK_NS.load_current();
                    let before_periodic = worker_periodic_ticks.load(Ordering::Acquire);
                    let remaining_ns = deadline_ns.saturating_sub(now_ns);
                    if remaining_ns < PERIODIC_TICK_NS / 3 {
                        (before_periodic, deadline_ns, false)
                    } else {
                        delivered.store(false, Ordering::Relaxed);
                        let at_ns = now_ns + remaining_ns / 4;
                        let at = ktime_types::MonotonicInstant::from_span_since_origin(
                            ktime_types::TimeSpan::from_nanos(at_ns),
                        );
                        // SAFETY: this initialized CPU remains pinned. The owned
                        // handler is bounded, IRQ-safe and stays live through cancel.
                        let started = unsafe { timer.start(at) }.is_ok();
                        if started {
                            // Arm the shared scheduler deadline for this same
                            // periodic tick while still IRQ-off. Observing the
                            // tick and arming it in two separate critical
                            // sections would race with the tick itself: an IRQ
                            // in between advances NEXT_PERIODIC_TICK_NS and the
                            // test would then assert the property it never
                            // exercised. Precondition and property stay separate:
                            // `shared_armed` below proves the arm happened.
                            NEXT_SCHED_DEADLINE_NS.store_current(deadline_ns);
                            rearm_local_timer();
                            shared_armed.store(true, Ordering::Release);
                        }
                        (before_periodic, deadline_ns, started)
                    }
                };
                if !started {
                    while worker_periodic_ticks.load(Ordering::Acquire) == before_periodic
                        && khal::time::monotonic_time_nanos() < limit_ns
                    {
                        core::hint::spin_loop();
                    }
                    continue;
                }
                while !delivered.load(Ordering::Acquire)
                    && khal::time::monotonic_time_nanos() < limit_ns
                {
                    core::hint::spin_loop();
                }
                if delivered.load(Ordering::Acquire)
                    && fired_ns.load(Ordering::Relaxed) < deadline_ns
                {
                    worker_valid.store(
                        observed_periodic_ticks.load(Ordering::Relaxed) == before_periodic
                            && observed_deadline.load(Ordering::Relaxed) == deadline_ns,
                        Ordering::Relaxed,
                    );
                    original_deadline_ns = deadline_ns;
                    worker_checked.store(true, Ordering::Release);
                    break;
                }
            }
            while NEXT_PERIODIC_TICK_NS.load_current() <= original_deadline_ns
                && khal::time::monotonic_time_nanos() < limit_ns
            {
                core::hint::spin_loop();
            }
            worker_advanced.store(
                original_deadline_ns != 0 && NEXT_PERIODIC_TICK_NS.load_current() > original_deadline_ns,
                Ordering::Release,
            );
            worker_shared.store(
                shared_armed.load(Ordering::Acquire)
                    && NEXT_SCHED_DEADLINE_NS.load_current() != original_deadline_ns,
                Ordering::Release,
            );
            worker_runtime.store(current().sum_exec_runtime_ns(), Ordering::Release);
            timer.cancel_sync();
        })
        .expect("test entry preparation"),
        "periodic-tick-probe".into(),
        kidentity::allocate_root_pid_handle().expect("test task identity"),
        KBox::new(RuntimeProbe {
            periodic_ticks: periodic_ticks.clone(),
            invalid_context: invalid_context.clone(),
        })
        .expect("test runtime allocation"),
    ));
    crate::activate_task(&task);
    assert_eq!(task.join(), 0);
    assert!(periodic_ticks.load(Ordering::Acquire) >= 2);
    assert!(!invalid_context.load(Ordering::Relaxed));
    assert!(checked_early_irq.load(Ordering::Acquire));
    assert!(early_irq_was_valid.load(Ordering::Relaxed));
    assert!(periodic_advanced.load(Ordering::Acquire));
    // The property below is only meaningful when the shared deadline was
    // actually armed for a periodic tick; keep the two failures distinguishable.
    assert!(shared_deadline_armed.load(Ordering::Acquire));
    assert!(shared_deadline_consumed.load(Ordering::Acquire));
    assert!(runtime_ns.load(Ordering::Acquire) > 0);
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

#[def_test(serial)]
fn task_preparation_accepts_bh_disabled_task_context() {
    let _bh = local_bh_disable();
    let task = crate::TaskInner::new_pidless_kthread_with_entry(
        crate::PreparedTaskEntry::new_nowait(|| {}).unwrap(),
        "bh-preparation".into(),
        16384,
    );
    let owner = super::prepare_task(task);
    assert_eq!(owner.state(), crate::TaskState::Ready);
    assert!(!can_sleep_current_context_for_tests());
    drop(owner);
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

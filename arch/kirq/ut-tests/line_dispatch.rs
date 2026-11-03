// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use unittest::def_test;

use super::{IrqDispatch, IrqDispatchGuard};
use crate::{IrqDesc, state::{IRQ_STATE, IrqLine, IrqStateDesc}};

static MASKS: AtomicUsize = AtomicUsize::new(0);
static UNMASKS: AtomicUsize = AtomicUsize::new(0);
static FLOW_CHIP: crate::FasteoiChip = crate::FasteoiChip {
    resend: crate::FasteoiResend::Software,
    mask: |_| { MASKS.fetch_add(1, Ordering::Relaxed); },
    unmask: |_| { UNMASKS.fetch_add(1, Ordering::Relaxed); },
};

fn fasteoi_line(virq: usize) -> Arc<IrqLine> {
    let mut entry = IrqStateDesc::new(IrqDesc::from_virq(virq));
    entry.flow = crate::IrqFlow::Fasteoi(&FLOW_CHIP);
    Arc::new(IrqLine::new(entry))
}

#[def_test(serial)]
fn legacy_percpu_overlapping_claims_retain_independent_admission() -> unittest::TestResult {
    use unittest::{assert, assert_eq};

    struct Observations {
        calls: AtomicUsize,
        nested_in_flight: AtomicUsize,
        inner_completion_in_flight: AtomicUsize,
        after_inner_in_flight: AtomicUsize,
    }
    let virq = 0x5f3b;
    let desc = IrqDesc::from_virq(virq).with_flags(crate::IrqFlags::PER_CPU);
    let line = Arc::new(IrqLine::new(IrqStateDesc::new(desc)));
    let has_atomic_exclusion = {
        let _guard = line.lock();
        line.is_state_locked_for_tests()
    };
    if !has_atomic_exclusion {
        return unittest::TestResult::Ignored;
    }
    let completion = line.lock().in_flight_zero_completion();
    let observations = Arc::new(Observations {
        calls: AtomicUsize::new(0),
        nested_in_flight: AtomicUsize::new(0),
        inner_completion_in_flight: AtomicUsize::new(0),
        after_inner_in_flight: AtomicUsize::new(0),
    });
    let weak = Arc::downgrade(&line);
    let observed = observations.clone();
    let drain = completion.clone();
    let handler = Arc::new(move |_| {
        let retained = weak.upgrade().unwrap();
        if observed.calls.fetch_add(1, Ordering::Relaxed) == 0 {
            // Model overlapping claims without a scheduler/time dependency.
            // This is not a hardware recursive IRQ or an SMP delivery test.
            super::dispatch_claim(crate::IrqRef::Virq(virq), || {
                // Only this fixture owns the synthetic Line. Check exclusion
                // before reading so a locked completion fails instead of hanging.
                if !drain.is_completed() && !retained.is_state_locked_for_tests() {
                    observed.inner_completion_in_flight.store(
                        retained.lock().in_flight(), Ordering::Relaxed,
                    );
                }
            });
            if !drain.is_completed() {
                observed.after_inner_in_flight.store(
                    retained.lock().in_flight(), Ordering::Relaxed,
                );
            }
        } else {
            observed.nested_in_flight.store(retained.lock().in_flight(), Ordering::Relaxed);
        }
        crate::IrqEvent::HANDLED
    });
    assert!(line.lock().install_regular_action(crate::action::IrqAction::regular(handler)));
    {
        let mut entry = line.lock();
        let plan = entry.prepare_auto_enable();
        entry.commit_platform_plan(plan);
    }
    assert!(IRQ_STATE.lock().descs.insert(virq, line.clone()).is_none());
    let mut outer_completion_in_flight = 0;
    let resolved = super::dispatch_claim(crate::IrqRef::Virq(virq), || {
        if !completion.is_completed() && !line.is_state_locked_for_tests() {
            outer_completion_in_flight = line.lock().in_flight();
        }
    });
    let removed = IRQ_STATE.lock().descs.remove(&virq);
    let remaining_in_flight = line.lock().in_flight();
    let is_running = line.lock().is_running;
    drop(removed);
    drop(line);

    assert_eq!(resolved, Some(virq));
    assert_eq!(observations.calls.load(Ordering::Relaxed), 2);
    assert_eq!(observations.nested_in_flight.load(Ordering::Relaxed), 2);
    assert_eq!(observations.inner_completion_in_flight.load(Ordering::Relaxed), 2);
    assert_eq!(observations.after_inner_in_flight.load(Ordering::Relaxed), 1);
    assert_eq!(outer_completion_in_flight, 1);
    assert_eq!(remaining_in_flight, 0);
    assert!(!is_running);
    assert!(completion.is_completed());
    unittest::TestResult::Ok
}

#[def_test(serial)]
fn disabled_fasteoi_masks_before_completion_without_running_actions() {
    let virq = 0x5f38;
    let line = fasteoi_line(virq);
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    assert!(line.lock().install_regular_action(crate::action::IrqAction::regular(Arc::new(move |_| {
        count.fetch_add(1, Ordering::Relaxed);
        crate::IrqEvent::HANDLED
    }))));
    {
        let mut entry = line.lock();
        let plan = entry.prepare_register_disabled();
        entry.commit_platform_plan(plan);
    }
    MASKS.store(0, Ordering::Relaxed);
    assert!(IRQ_STATE.lock().descs.insert(virq, line.clone()).is_none());
    let mut completions = 0;
    assert_eq!(super::dispatch_claim(crate::IrqRef::Virq(virq), || {
        assert_eq!(MASKS.load(Ordering::Relaxed), 1);
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        completions += 1;
    }), Some(virq));
    assert_eq!(completions, 1);
    assert!(line.lock().is_pending);
    assert!(!line.lock().is_running);
    assert_eq!(line.lock().in_flight(), 0);
    IRQ_STATE.lock().descs.remove(&virq);
}

#[def_test(serial)]
fn concurrent_fasteoi_claim_does_not_clear_its_running_owner() {
    let virq = 0x5f39;
    let line = fasteoi_line(virq);
    line.lock().is_running = true;
    line.lock().begin_dispatch();
    MASKS.store(0, Ordering::Relaxed);
    assert!(IRQ_STATE.lock().descs.insert(virq, line.clone()).is_none());
    let mut completions = 0;
    super::dispatch_claim(crate::IrqRef::Virq(virq), || completions += 1);
    assert_eq!(completions, 1);
    assert_eq!(MASKS.load(Ordering::Relaxed), 0);
    assert!(line.lock().is_running);
    assert!(!line.lock().is_pending);
    assert_eq!(line.lock().in_flight(), 1);
    line.lock().is_running = false;
    drop(IrqDispatchGuard::new(virq, line));
    assert!(!IRQ_STATE.lock().descs.contains_key(&virq));
}

#[def_test(serial)]
fn handler_disable_survives_fasteoi_tail() {
    let virq = 0x5f3a;
    let line = fasteoi_line(virq);
    let weak = Arc::downgrade(&line);
    assert!(line.lock().install_regular_action(crate::action::IrqAction::regular(Arc::new(move |_| {
        let retained = weak.upgrade().unwrap();
        let mut entry = retained.lock();
        assert!(entry.is_running);
        let plan = entry.prepare_disable_irq_nosync();
        entry.commit_platform_plan(plan);
        crate::IrqEvent::HANDLED
    }))));
    {
        let mut entry = line.lock();
        let plan = entry.prepare_auto_enable();
        entry.commit_platform_plan(plan);
    }
    UNMASKS.store(0, Ordering::Relaxed);
    assert!(IRQ_STATE.lock().descs.insert(virq, line.clone()).is_none());
    let completion = line.lock().in_flight_zero_completion();
    super::dispatch_claim(crate::IrqRef::Virq(virq), || {
        assert!(!completion.is_completed());
    });
    assert!(!line.lock().can_run_actions());
    assert!(!line.lock().is_running);
    assert!(completion.is_completed());
    assert_eq!(UNMASKS.load(Ordering::Relaxed), 0);
    IRQ_STATE.lock().descs.remove(&virq);
}

#[def_test(serial)]
fn dispatch_guard_finishes_original_line_not_replacement() {
    let virq = 0x5f34;
    let original = Arc::new(IrqLine::new(IrqStateDesc::new(IrqDesc::from_virq(virq))));
    let replacement = Arc::new(IrqLine::new(IrqStateDesc::new(IrqDesc::from_virq(virq))));
    original.lock().begin_dispatch();
    let guard = IrqDispatchGuard::new(virq, original.clone());

    // Deliberately bypass the production zero-in-flight removal gate: this
    // fixture tests the guard's ownership independently of directory policy.
    assert!(IRQ_STATE.lock().descs.insert(virq, replacement.clone()).is_none());
    drop(guard);

    let stored = IRQ_STATE.lock().descs.remove(&virq);
    assert!(stored.as_ref().is_some_and(|line| Arc::ptr_eq(line, &replacement)));
    assert_eq!(original.lock().in_flight(), 0);
    assert!(original.lock().in_flight_zero_completion().is_completed());
    assert_eq!(replacement.lock().in_flight(), 0);
}

#[def_test(serial)]
fn dispatch_guard_retains_line_until_completion() {
    let line = Arc::new(IrqLine::new(IrqStateDesc::new(IrqDesc::from_virq(0x5f35))));
    line.lock().begin_dispatch();
    let completion = line.lock().in_flight_zero_completion();
    let weak = Arc::downgrade(&line);
    let guard = IrqDispatchGuard::new(0x5f35, line.clone());
    drop(line);

    assert!(weak.upgrade().is_some());
    assert!(!completion.is_completed());
    drop(guard);
    assert!(completion.is_completed());
    assert!(weak.upgrade().is_none());
}

#[def_test(serial)]
fn dispatch_completion_keeps_admission_until_callback_returns() {
    let virq = 0x5f36;
    let line = Arc::new(IrqLine::new(IrqStateDesc::new(IrqDesc::from_virq(virq))));
    line.lock().begin_dispatch();
    let completion = line.lock().in_flight_zero_completion();
    let dispatch = IrqDispatch {
        resolved_irq: Some(virq),
        guard: Some(IrqDispatchGuard::new(virq, line.clone())),
        owns_running: false,
    };
    let mut calls = 0;
    let resolved = dispatch.complete(|| {
        calls += 1;
        assert_eq!(line.lock().in_flight(), 1);
        assert!(!completion.is_completed());
    });
    assert_eq!(resolved, Some(virq));
    assert_eq!(calls, 1);
    assert_eq!(line.lock().in_flight(), 0);
    assert!(completion.is_completed());
}

#[def_test]
fn dispatch_without_line_still_completes_once() {
    for resolved_irq in [None, Some(0x5f37)] {
        let dispatch = IrqDispatch { resolved_irq, guard: None, owns_running: false };
        let mut calls = 0;
        assert_eq!(dispatch.complete(|| calls += 1), resolved_irq);
        assert_eq!(calls, 1);
    }
}

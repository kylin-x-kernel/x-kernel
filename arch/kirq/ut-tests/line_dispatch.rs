// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use unittest::def_test;

use super::{IrqDispatch, IrqDispatchGuard};
use crate::{
    IrqDesc,
    state::{IRQ_STATE, IrqLine, IrqStateDesc},
};

static MASKS: AtomicUsize = AtomicUsize::new(0);
static UNMASKS: AtomicUsize = AtomicUsize::new(0);
static FLOW_CHIP: crate::FasteoiChip = crate::FasteoiChip {
    resend: crate::FasteoiResend::Software,
    mask: |_| {
        MASKS.fetch_add(1, Ordering::Relaxed);
    },
    unmask: |_| {
        UNMASKS.fetch_add(1, Ordering::Relaxed);
    },
};

fn fasteoi_line(virq: usize) -> Arc<IrqLine> {
    let mut entry = IrqStateDesc::test_new(IrqDesc::from_virq(virq));
    entry.set_flow_for_tests(crate::IrqFlow::Fasteoi(&FLOW_CHIP));
    Arc::new(IrqLine::new(entry))
}

#[def_test(serial)]
fn rejected_flows_complete_under_line_lock_without_execution_admission() -> unittest::TestResult {
    use unittest::{assert, assert_eq};

    static LEVEL_CHIP: crate::LevelChip = crate::LevelChip {
        mask_ack: |_| {
            MASKS.fetch_add(1, Ordering::Relaxed);
        },
        unmask: |_| {
            UNMASKS.fetch_add(1, Ordering::Relaxed);
        },
    };
    static EDGE_CHIP: crate::EdgeChip = crate::EdgeChip {
        mask: |_| {
            MASKS.fetch_add(1, Ordering::Relaxed);
        },
        unmask: |_| {
            UNMASKS.fetch_add(1, Ordering::Relaxed);
        },
        retrigger: |_| panic!("rejected claim must not request another delivery"),
    };
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Rejection {
        Empty,
        Disabled,
        Running,
    }

    let virq = 0x5f3e;
    for flow in [
        crate::IrqFlow::Level(&LEVEL_CHIP),
        crate::IrqFlow::Edge(&EDGE_CHIP),
        crate::IrqFlow::Fasteoi(&FLOW_CHIP),
    ] {
        for rejection in [Rejection::Empty, Rejection::Disabled, Rejection::Running] {
            let calls = Arc::new(AtomicUsize::new(0));
            let observed = calls.clone();
            let mut entry = IrqStateDesc::test_new(IrqDesc::from_virq(virq));
            entry.set_flow_for_tests(flow);
            if rejection != Rejection::Empty {
                assert!(
                    entry.install_regular_action(crate::action::IrqAction::regular(Arc::new(
                        move |_| {
                            observed.fetch_add(1, Ordering::Relaxed);
                            crate::IrqEvent::HANDLED
                        }
                    ),))
                );
            }
            let plan = if rejection == Rejection::Disabled {
                entry.prepare_register_disabled()
            } else {
                entry.prepare_auto_enable()
            };
            entry.commit_platform_plan(plan);
            // Model only the running rejection gate, with no synthetic owner
            // credit, so the drain snapshot detects admission at completion.
            entry.is_running = rejection == Rejection::Running;
            let line = Arc::new(IrqLine::new(entry));
            let has_atomic_exclusion = {
                let _guard = line.lock();
                line.is_state_locked_for_tests()
            };
            if !has_atomic_exclusion {
                return unittest::TestResult::Ignored;
            }
            let completion = line.lock().execution_idle_completion();
            assert!(completion.is_completed());
            assert!(IRQ_STATE.lock().descs.insert(virq, line.clone()).is_none());
            MASKS.store(0, Ordering::Relaxed);
            UNMASKS.store(0, Ordering::Relaxed);
            let mut completions = 0;
            let mut is_completion_locked = false;
            let mut is_drain_completed = false;
            let mut is_irq_disabled = false;
            let mut masks_at_completion = 0;
            let resolved = super::dispatch_claim(crate::IrqRef::Virq(virq), || {
                completions += 1;
                // Never relock the Line here: rejection must retain it through
                // the original claim's completion. This snapshot has its own lock.
                is_completion_locked = line.is_state_locked_for_tests();
                is_drain_completed = completion.is_completed();
                is_irq_disabled = !karch::local_irq_enabled();
                masks_at_completion = MASKS.load(Ordering::Relaxed);
            });
            // Remove fixture membership before assertions that can return early.
            let removed = IRQ_STATE.lock().descs.remove(&virq);
            assert!(
                removed
                    .as_ref()
                    .is_some_and(|stored| Arc::ptr_eq(stored, &line))
            );
            let expected_masks = usize::from(
                !matches!(flow, crate::IrqFlow::Fasteoi(_)) || rejection != Rejection::Running,
            );
            assert_eq!(resolved, Some(virq));
            assert_eq!(completions, 1);
            assert!(is_completion_locked);
            assert!(is_drain_completed);
            assert!(is_irq_disabled);
            assert_eq!(masks_at_completion, expected_masks);
            assert_eq!(MASKS.load(Ordering::Relaxed), expected_masks);
            assert_eq!(UNMASKS.load(Ordering::Relaxed), 0);
            assert_eq!(calls.load(Ordering::Relaxed), 0);
            let entry = line.lock();
            assert!(completion.is_completed());
            assert_eq!(entry.is_running, rejection == Rejection::Running);
            assert_eq!(entry.is_pending, expected_masks != 0);
        }
    }
    unittest::TestResult::Ok
}

#[def_test(serial)]
fn retired_lookup_completes_claim_without_old_flow_operations() {
    use unittest::{assert, assert_eq};

    struct RestoreLine {
        virq: usize,
        previous: Option<Arc<IrqLine>>,
    }
    impl Drop for RestoreLine {
        fn drop(&mut self) {
            let removed = {
                let mut state = IRQ_STATE.lock();
                match self.previous.take() {
                    Some(previous) => state.descs.insert(self.virq, previous),
                    None => state.descs.remove(&self.virq),
                }
            };
            drop(removed);
        }
    }

    let virq = 0x5f3c;
    let line = fasteoi_line(virq);
    let mut private = crate::state::IrqState::new();
    private.descs.insert(virq, line.clone());
    assert!(private.remove_if_unused(virq).is_some());
    assert!(line.lock().is_retired());

    // Model a lookup retained across withdrawal. Current registry exclusion
    // prevents this interleaving; a future RCU lookup must preserve rejection.
    let previous = IRQ_STATE.lock().descs.insert(virq, line.clone());
    let restore = RestoreLine { virq, previous };
    assert!(restore.previous.is_none());
    MASKS.store(0, Ordering::Relaxed);
    UNMASKS.store(0, Ordering::Relaxed);
    let mut completions = 0;
    let resolved = super::dispatch_claim(crate::IrqRef::Virq(virq), || {
        completions += 1;
    });
    drop(restore);

    assert_eq!(resolved, Some(virq));
    assert_eq!(completions, 1);
    assert_eq!(MASKS.load(Ordering::Relaxed), 0);
    assert_eq!(UNMASKS.load(Ordering::Relaxed), 0);
    let entry = line.lock();
    assert!(!entry.is_running);
    assert!(!entry.is_pending);
}

#[def_test(serial)]
fn percpu_nested_claims_complete_independently() {
    let _pin = kspin::NoPreemptIrqSave::new();
    let virq = 0x5f3b;
    let mut entry = IrqStateDesc::test_new(IrqDesc::from_virq(virq));
    entry.set_flow_for_tests(crate::IrqFlow::PerCpu);
    let line = Arc::new(IrqLine::new(entry));
    let calls = Arc::new(AtomicUsize::new(0));
    let completions = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let completed = completions.clone();
    let weak = Arc::downgrade(&line);
    let handler: crate::Handler = Arc::new(move |_| {
        let line = weak.upgrade().unwrap();
        assert!(!line.is_state_locked_for_tests());
        assert!(!line.lock().is_running);
        if observed.fetch_add(1, Ordering::Relaxed) == 0 {
            // Private fixture dispatch can nest; this does not model hardware nesting.
            super::dispatch_claim(crate::IrqRef::Virq(virq), || {
                assert!(!line.is_state_locked_for_tests());
                assert_eq!(observed.load(Ordering::Relaxed), 2);
                completed.fetch_add(1, Ordering::Relaxed);
            });
            assert_eq!(completed.load(Ordering::Relaxed), 1);
        }
        crate::IrqEvent::HANDLED
    });
    {
        let mut entry = line.lock();
        assert!(entry.install_regular_action(crate::action::IrqAction::regular(handler.clone())));
        let plan = entry.prepare_auto_enable();
        entry.commit_platform_plan(plan);
    }
    assert!(IRQ_STATE.lock().descs.insert(virq, line.clone()).is_none());
    let resolved = super::dispatch_claim(crate::IrqRef::Virq(virq), || {
        assert!(!line.is_state_locked_for_tests());
        assert_eq!(completions.load(Ordering::Relaxed), 1);
        assert_eq!(Arc::strong_count(&handler), 2);
        completions.fetch_add(1, Ordering::Relaxed);
    });
    let removed = IRQ_STATE.lock().descs.remove(&virq);
    assert_eq!(resolved, Some(virq));
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    assert_eq!(completions.load(Ordering::Relaxed), 2);
    assert!(!line.lock().is_running);
    // No hardware producer exists, both synthetic dispatch stacks have returned,
    // and this pinned fixture is the sole starter. Stop delivery before unlink.
    let retired = {
        let mut entry = line.lock();
        let plan = entry.prepare_disable_irq_nosync();
        entry.commit_platform_plan(plan);
        entry.withdraw_action(None).unwrap()
    };
    // SAFETY: the sole starter completed both claims and disabled its only local
    // slot before withdrawal. No snapshot or completion remains and no restart
    // can occur on this unpublished, exclusively owned fixture.
    drop(unsafe { retired.reclaim() });
    assert_eq!(Arc::strong_count(&handler), 1);
    drop(removed);
}

#[def_test(serial)]
fn percpu_self_disable_retains_owner_until_source_shutdown_completes() {
    let _pin = kspin::NoPreemptIrqSave::new();
    let virq = 0x5f40;
    let mut entry = IrqStateDesc::test_new(IrqDesc::from_virq(virq));
    entry.set_flow_for_tests(crate::IrqFlow::PerCpu);
    let line = Arc::new(IrqLine::new(entry));
    let weak = Arc::downgrade(&line);
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let handler: crate::Handler = Arc::new(move |_| {
        let retained = weak.upgrade().unwrap();
        assert!(!retained.is_state_locked_for_tests());
        let mut entry = retained.lock();
        let plan = entry.prepare_disable_irq_nosync();
        entry.commit_platform_plan(plan);
        observed.fetch_add(1, Ordering::Relaxed);
        crate::IrqEvent::HANDLED
    });
    {
        let mut entry = line.lock();
        assert!(entry.install_regular_action(crate::action::IrqAction::regular(handler.clone())));
        let plan = entry.prepare_auto_enable();
        entry.commit_platform_plan(plan);
    }
    assert!(IRQ_STATE.lock().descs.insert(virq, line.clone()).is_none());
    let mut completions = 0;
    super::dispatch_claim(crate::IrqRef::Virq(virq), || {
        completions += 1;
        assert!(!line.is_state_locked_for_tests());
        let entry = line.lock();
        assert!(!entry.has_enabled_local_cpu());
        assert!(!entry.can_run_local_actions());
        assert_eq!(entry.action_count(), 1);
        assert_eq!(Arc::strong_count(&handler), 2);
        // An empty enabled set is not permission to reclaim inside this callback:
        // original completion has not returned to the source owner yet.
    });
    super::dispatch_claim(crate::IrqRef::Virq(virq), || completions += 1);
    let removed = IRQ_STATE.lock().descs.remove(&virq);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(completions, 2);
    assert_eq!(Arc::strong_count(&handler), 2);
    let retired = line.lock().withdraw_action(None).unwrap();
    // SAFETY: this fixture has no hardware producer or other starters. Both
    // dispatch/completion stacks have returned, the sole local slot is disabled,
    // and directory removal plus exclusive fixture ownership excludes restart.
    drop(unsafe { retired.reclaim() });
    assert_eq!(Arc::strong_count(&handler), 1);
    drop(removed);
}

#[def_test(serial)]
fn rejected_percpu_claim_completes_locked_without_execution_admission() {
    use unittest::{assert, assert_eq};

    #[derive(Clone, Copy)]
    enum Rejection {
        Empty,
        Disabled,
        ConfigurationFailed,
        Teardown,
    }

    let _pin = kspin::NoPreemptIrqSave::new();
    let virq = 0x5f4f;
    for rejection in [
        Rejection::Empty,
        Rejection::Disabled,
        Rejection::ConfigurationFailed,
        Rejection::Teardown,
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let mut entry = IrqStateDesc::test_new(IrqDesc::from_virq(virq));
        entry.set_flow_for_tests(crate::IrqFlow::PerCpu);
        if !matches!(rejection, Rejection::Empty) {
            assert!(
                entry.install_regular_action(crate::action::IrqAction::regular(Arc::new(
                    move |_| {
                        observed.fetch_add(1, Ordering::Relaxed);
                        crate::IrqEvent::HANDLED
                    }
                ),))
            );
        }
        let plan = entry.prepare_auto_enable();
        entry.commit_platform_plan(plan);
        match rejection {
            Rejection::Disabled => {
                let plan = entry.prepare_disable_irq_nosync();
                entry.commit_platform_plan(plan);
            }
            Rejection::ConfigurationFailed => entry.mark_configuration_failed(),
            Rejection::Teardown => entry.begin_teardown(),
            Rejection::Empty => {}
        }
        let snapshot = |entry: &IrqStateDesc| {
            let plan = entry.prepare_enable_irq();
            (
                entry.desc,
                entry.flow,
                entry.action_count(),
                entry.has_enabled_local_cpu(),
                entry.is_teardown_in_progress(),
                entry.is_pending,
                plan.configure,
                plan.enable,
                entry.needs_configuration_restore(),
            )
        };
        assert!(!entry.can_run_local_actions());
        let before = snapshot(&entry);
        let completion = entry.execution_idle_completion();
        assert!(completion.is_completed());
        let line = Arc::new(IrqLine::new(entry));
        let has_atomic_exclusion = {
            let _guard = line.lock();
            line.is_state_locked_for_tests()
        };
        assert!(IRQ_STATE.lock().descs.insert(virq, line.clone()).is_none());
        let mut completions = 0;
        let mut is_completion_protected = false;
        let resolved = super::dispatch_claim(crate::IrqRef::Virq(virq), || {
            completions += 1;
            // Never relock the Line. UP may expose no atomic lock state;
            // there the exclusion assertion proves only IRQ masking.
            is_completion_protected = !karch::local_irq_enabled()
                && (!has_atomic_exclusion || line.is_state_locked_for_tests())
                && completion.is_completed();
        });
        // No assertion can return with this fixture still published. The
        // retained owners are dropped outside registry and Line guards.
        let removed = IRQ_STATE.lock().descs.remove(&virq);
        assert!(
            removed
                .as_ref()
                .is_some_and(|stored| Arc::ptr_eq(stored, &line))
        );
        assert_eq!(resolved, Some(virq));
        assert_eq!(completions, 1);
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert!(is_completion_protected);
        let entry = line.lock();
        assert_eq!(snapshot(&entry), before);
        assert!(!entry.can_run_local_actions());
        assert!(!entry.is_running);
        assert!(completion.is_completed());
        drop(entry);
        assert!(completion.is_completed());
    }
}

#[def_test(serial)]
fn ordinary_rejection_completes_locked_without_running_actions() {
    use unittest::{assert, assert_eq};

    #[derive(Clone, Copy)]
    enum State {
        Enabled,
        DisabledRegistration,
        DisabledAfterEnable,
        ConfigurationFailed,
        Teardown,
    }

    let virq = 0x5f3f;
    for state in [
        State::Enabled,
        State::DisabledRegistration,
        State::DisabledAfterEnable,
        State::ConfigurationFailed,
        State::Teardown,
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let mut entry = IrqStateDesc::test_new(IrqDesc::from_virq(virq));
        assert!(
            entry.install_regular_action(crate::action::IrqAction::regular(Arc::new(move |_| {
                observed.fetch_add(1, Ordering::Relaxed);
                crate::IrqEvent::HANDLED
            }),))
        );
        let plan = if matches!(state, State::DisabledRegistration) {
            entry.prepare_register_disabled()
        } else {
            entry.prepare_auto_enable()
        };
        entry.commit_platform_plan(plan);
        match state {
            State::DisabledAfterEnable => {
                let plan = entry.prepare_disable_irq_nosync();
                entry.commit_platform_plan(plan);
            }
            State::ConfigurationFailed => entry.mark_configuration_failed(),
            State::Teardown => entry.begin_teardown(),
            State::Enabled | State::DisabledRegistration => {}
        }
        let line = Arc::new(IrqLine::new(entry));
        let completion = line.lock().execution_idle_completion();
        assert!(IRQ_STATE.lock().descs.insert(virq, line.clone()).is_none());
        let mut completions = 0;
        let has_atomic_exclusion = {
            let _guard = line.lock();
            line.is_state_locked_for_tests()
        };
        let mut has_locked_completion = false;
        let mut has_expected_idle_state = false;
        let resolved = super::dispatch_claim(crate::IrqRef::Virq(virq), || {
            completions += 1;
            has_locked_completion = !karch::local_irq_enabled()
                && (!has_atomic_exclusion || line.is_state_locked_for_tests());
            has_expected_idle_state = completion.is_completed() != matches!(state, State::Enabled);
        });
        let removed = IRQ_STATE.lock().descs.remove(&virq);
        assert!(
            removed
                .as_ref()
                .is_some_and(|stored| Arc::ptr_eq(stored, &line))
        );
        assert_eq!(resolved, Some(virq));
        assert_eq!(
            calls.load(Ordering::Relaxed),
            usize::from(matches!(state, State::Enabled))
        );
        assert_eq!(completions, 1);
        assert!(has_locked_completion);
        assert!(has_expected_idle_state);
        assert!(completion.is_completed());
    }
}

#[def_test(serial)]
fn disabled_fasteoi_masks_before_completion_without_running_actions() {
    let virq = 0x5f38;
    let line = fasteoi_line(virq);
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    assert!(
        line.lock()
            .install_regular_action(crate::action::IrqAction::regular(Arc::new(move |_| {
                count.fetch_add(1, Ordering::Relaxed);
                crate::IrqEvent::HANDLED
            })))
    );
    {
        let mut entry = line.lock();
        let plan = entry.prepare_register_disabled();
        entry.commit_platform_plan(plan);
    }
    MASKS.store(0, Ordering::Relaxed);
    assert!(IRQ_STATE.lock().descs.insert(virq, line.clone()).is_none());
    let mut completions = 0;
    assert_eq!(
        super::dispatch_claim(crate::IrqRef::Virq(virq), || {
            assert_eq!(MASKS.load(Ordering::Relaxed), 1);
            assert_eq!(calls.load(Ordering::Relaxed), 0);
            completions += 1;
        }),
        Some(virq)
    );
    assert_eq!(completions, 1);
    assert!(line.lock().is_pending);
    assert!(!line.lock().is_running);
    IRQ_STATE.lock().descs.remove(&virq);
}

#[def_test(serial)]
fn concurrent_fasteoi_claim_does_not_clear_its_running_owner() {
    let virq = 0x5f39;
    let line = fasteoi_line(virq);
    line.lock().begin_running_dispatch();
    MASKS.store(0, Ordering::Relaxed);
    assert!(IRQ_STATE.lock().descs.insert(virq, line.clone()).is_none());
    let mut completions = 0;
    super::dispatch_claim(crate::IrqRef::Virq(virq), || completions += 1);
    assert_eq!(completions, 1);
    assert_eq!(MASKS.load(Ordering::Relaxed), 0);
    assert!(line.lock().is_running);
    assert!(!line.lock().is_pending);
    assert!(!line.lock().execution_idle_completion().is_completed());
    drop(IrqDispatchGuard::new_running(virq, line.clone()));
    let retained = IRQ_STATE.lock().descs.remove(&virq);
    assert!(
        retained
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &line))
    );
    assert!(line.lock().execution_idle_completion().is_completed());
}

#[def_test(serial)]
fn handler_disable_survives_fasteoi_tail() {
    let virq = 0x5f3a;
    let line = fasteoi_line(virq);
    let weak = Arc::downgrade(&line);
    assert!(
        line.lock()
            .install_regular_action(crate::action::IrqAction::regular(Arc::new(move |_| {
                let retained = weak.upgrade().unwrap();
                let mut entry = retained.lock();
                assert!(entry.is_running);
                assert!(entry.has_active_execution());
                assert!(!entry.execution_idle_completion().is_completed());
                let plan = entry.prepare_disable_irq_nosync();
                entry.commit_platform_plan(plan);
                crate::IrqEvent::HANDLED
            })))
    );
    {
        let mut entry = line.lock();
        let plan = entry.prepare_auto_enable();
        entry.commit_platform_plan(plan);
    }
    UNMASKS.store(0, Ordering::Relaxed);
    assert!(IRQ_STATE.lock().descs.insert(virq, line.clone()).is_none());
    let completion = line.lock().execution_idle_completion();
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
    let original = Arc::new(IrqLine::new(IrqStateDesc::test_new(IrqDesc::from_virq(
        virq,
    ))));
    let replacement = Arc::new(IrqLine::new(IrqStateDesc::test_new(IrqDesc::from_virq(
        virq,
    ))));
    original.lock().begin_running_dispatch();
    let guard = IrqDispatchGuard::new_running(virq, original.clone());

    // Deliberately bypass the production running-owner removal gate: this
    // fixture tests the guard's ownership independently of directory policy.
    assert!(
        IRQ_STATE
            .lock()
            .descs
            .insert(virq, replacement.clone())
            .is_none()
    );
    drop(guard);

    let stored = IRQ_STATE.lock().descs.remove(&virq);
    assert!(
        stored
            .as_ref()
            .is_some_and(|line| Arc::ptr_eq(line, &replacement))
    );
    assert!(!original.lock().is_running);
    assert!(!replacement.lock().is_running);
    assert!(original.lock().execution_idle_completion().is_completed());
}

#[def_test(serial)]
fn percpu_guard_retains_line_without_running_admission() {
    let mut entry = IrqStateDesc::test_new(IrqDesc::from_virq(0x5f35));
    entry.set_flow_for_tests(crate::IrqFlow::PerCpu);
    let line = Arc::new(IrqLine::new(entry));
    let completion = line.lock().execution_idle_completion();
    let weak = Arc::downgrade(&line);
    let guard = IrqDispatchGuard::new_percpu(0x5f35, line.clone());
    drop(line);

    assert!(weak.upgrade().is_some());
    assert!(completion.is_completed());
    drop(guard);
    assert!(completion.is_completed());
    assert!(weak.upgrade().is_none());
}

#[def_test(serial)]
fn dispatch_completion_keeps_admission_until_callback_returns() {
    let virq = 0x5f36;
    let line = Arc::new(IrqLine::new(IrqStateDesc::test_new(IrqDesc::from_virq(
        virq,
    ))));
    line.lock().begin_running_dispatch();
    let completion = line.lock().execution_idle_completion();
    let dispatch = IrqDispatch {
        resolved_irq: Some(virq),
        guard: Some(IrqDispatchGuard::new_running(virq, line.clone())),
    };
    let mut calls = 0;
    let resolved = dispatch.complete(|| {
        calls += 1;
        assert!(!completion.is_completed());
    });
    assert_eq!(resolved, Some(virq));
    assert_eq!(calls, 1);
    assert!(completion.is_completed());
}

#[def_test]
fn dispatch_without_line_still_completes_once() {
    for resolved_irq in [None, Some(0x5f37)] {
        let dispatch = IrqDispatch {
            resolved_irq,
            guard: None,
        };
        let mut calls = 0;
        assert_eq!(dispatch.complete(|| calls += 1), resolved_irq);
        assert_eq!(calls, 1);
    }
}

#[def_test(serial)]
fn action_owner_remains_installed_through_original_claim_completion() {
    let _pin = kspin::NoPreemptIrqSave::new();
    for (virq, flow) in [
        (0x5f30, crate::IrqFlow::PerCpu),
        (0x5f31, crate::IrqFlow::Fasteoi(&FLOW_CHIP)),
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let mut entry = IrqStateDesc::test_new(IrqDesc::from_virq(virq));
        entry.set_flow_for_tests(flow);
        let line = Arc::new(IrqLine::new(entry));
        let weak = Arc::downgrade(&line);
        let handler: crate::platform::Handler = Arc::new(move |_| {
            assert!(!krcu::is_reading());
            let line = weak.upgrade().unwrap();
            assert!(!line.is_state_locked_for_tests());
            let entry = line.lock();
            let is_percpu = flow == crate::IrqFlow::PerCpu;
            assert_eq!(entry.is_running, !is_percpu);
            assert_eq!(entry.has_active_execution(), !is_percpu);
            assert_eq!(entry.execution_idle_completion().is_completed(), is_percpu);
            observed.fetch_add(1, Ordering::Relaxed);
            crate::IrqEvent::HANDLED
        });
        {
            let mut entry = line.lock();
            assert!(
                entry.install_regular_action(crate::action::IrqAction::regular(handler.clone()))
            );
            let plan = entry.prepare_auto_enable();
            entry.commit_platform_plan(plan);
        }
        let completion = line.lock().execution_idle_completion();
        assert!(IRQ_STATE.lock().descs.insert(virq, line.clone()).is_none());
        let mut completions = 0;
        super::dispatch_claim(crate::IrqRef::Virq(virq), || {
            assert_eq!(calls.load(Ordering::Relaxed), 1);
            assert!(!krcu::is_reading());
            assert_eq!(completion.is_completed(), flow == crate::IrqFlow::PerCpu);
            assert_eq!(Arc::strong_count(&handler), 2);
            completions += 1;
        });
        if flow == crate::IrqFlow::PerCpu {
            // This pinned fixture is the only starter and has no hardware
            // producer. Its dispatch and completion returned before local stop.
            let mut entry = line.lock();
            let plan = entry.prepare_disable_irq_nosync();
            entry.commit_platform_plan(plan);
        }
        let removed = IRQ_STATE.lock().descs.remove(&virq);
        drop(removed);
        assert_eq!(completions, 1);
        assert!(completion.is_completed());
        assert!(!line.lock().has_active_execution());
    }
}

struct RunningDrainWake(AtomicUsize);

// SAFETY: notification only increments an atomic; it cannot allocate, unwind,
// reenter the completion, or retain the waiter's borrowed reference.
unsafe impl kcompletion::SyncWake for RunningDrainWake {
    fn notify(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[def_test]
fn running_dispatch_resets_and_wakes_execution_completion() {
    let mut entry = IrqStateDesc::test_new(IrqDesc::from_virq(0x5f64));
    let completion = entry.execution_idle_completion();
    let notified = RunningDrainWake(AtomicUsize::new(0));
    for cycle in 0..2 {
        assert!(!entry.has_active_execution());
        assert!(completion.is_completed());
        entry.begin_running_dispatch();
        assert!(entry.is_running);
        assert!(entry.has_active_execution());
        assert!(!completion.is_completed());
        let mut finish = || {
            let wake = entry.finish_running_dispatch().unwrap();
            assert!(!entry.is_running);
            assert!(!entry.has_active_execution());
            assert!(completion.is_completed());
            assert_eq!(notified.0.load(Ordering::Relaxed), cycle);
            assert_eq!(wake.wake(), 1);
        };
        // SAFETY: finish returns or unwinds normally, preserving the scoped
        // waiter cleanup before notified and completion leave this stack.
        unsafe { completion.wait_with(&notified, &mut finish) };
        assert_eq!(notified.0.load(Ordering::Relaxed), cycle + 1);
        assert!(completion.is_completed());
    }
}

#[def_test]
fn old_running_wake_does_not_complete_a_successor() {
    let mut entry = IrqStateDesc::test_new(IrqDesc::from_virq(0x5f66));
    let completion = entry.execution_idle_completion();
    entry.begin_running_dispatch();
    let old_wake = entry.finish_running_dispatch().unwrap();
    assert!(completion.is_completed());
    entry.begin_running_dispatch();
    assert!(!completion.is_completed());
    old_wake.wake();
    assert!(entry.is_running);
    assert!(!completion.is_completed());
    entry.finish_running_dispatch().unwrap().wake();
    assert!(!entry.is_running);
    assert!(completion.is_completed());
}

#[def_test]
fn finished_guard_cannot_clear_a_successor_running_owner() {
    let virq = 0x5f68;
    let line = fasteoi_line(virq);
    line.lock().begin_running_dispatch();
    let mut guard = IrqDispatchGuard::new_running(virq, line.clone());
    let completion = line.lock().execution_idle_completion();
    let finished = {
        let mut entry = line.lock();
        super::finish_admission(&mut entry, &mut guard.admission, false)
    };
    line.lock().begin_running_dispatch();
    drop(guard);
    super::report_finished(virq, finished);
    {
        let entry = line.lock();
        assert!(entry.is_running);
        assert!(entry.has_active_execution());
        assert!(!completion.is_completed());
    }
    let wake = line.lock().finish_running_dispatch().unwrap();
    wake.wake();
    assert!(completion.is_completed());
}

#[def_test]
fn running_admission_retains_a_withdrawn_raw_snapshot() {
    let virq = 0x5f67;
    let line = fasteoi_line(virq);
    let weak = Arc::downgrade(&line);
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let handler: crate::Handler = Arc::new(move |_| {
        let line = weak.upgrade().unwrap();
        let entry = line.lock();
        assert!(entry.is_running);
        assert!(entry.has_active_execution());
        assert!(!entry.execution_idle_completion().is_completed());
        observed.fetch_add(1, Ordering::Relaxed);
        crate::IrqEvent::HANDLED
    });
    assert!(
        line.lock()
            .install_regular_action(crate::action::IrqAction::regular(handler.clone()))
    );
    let pin = kspin::NoPreemptIrqSave::new();
    let mut entry = line.lock();
    entry.begin_running_dispatch();
    let completion = entry.execution_idle_completion();
    // SAFETY: this fixture retains line and its running admission until after
    // snapshot consumption; withdrawal cannot reclaim the detached owner yet.
    let snapshot = unsafe { entry.snapshot_actions() };
    drop(entry);
    let retired = line.lock().withdraw_action(None).unwrap();
    assert!(!line.lock().has_actions());
    assert!(line.lock().has_active_execution());
    assert!(!completion.is_completed());
    assert_eq!(Arc::strong_count(&handler), 2);
    assert!(!super::run_actions(virq, snapshot));
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    let wake = {
        let mut entry = line.lock();
        assert!(entry.is_running);
        assert!(!completion.is_completed());
        entry.finish_running_dispatch().unwrap()
    };
    wake.wake();
    drop(pin);
    assert!(!line.lock().has_active_execution());
    assert!(completion.is_completed());
    // SAFETY: the sole raw snapshot was consumed before running admission
    // ended. The owner is unlinked; no later capture can reach it.
    let removed = unsafe { retired.reclaim() };
    drop(removed);
    assert_eq!(Arc::strong_count(&handler), 1);
}

#[def_test(serial)]
fn captured_action_chain_survives_unlink_before_traversal() {
    let virq = 0x5f3d;
    let removed_calls = Arc::new(AtomicUsize::new(0));
    let survivor_calls = Arc::new(AtomicUsize::new(0));
    let observed = removed_calls.clone();
    let removed_owner: crate::Handler = Arc::new(move |_| {
        observed.fetch_add(1, Ordering::Relaxed);
        crate::IrqEvent::HANDLED
    });
    let observed = survivor_calls.clone();
    let survivor_owner: crate::Handler = Arc::new(move |_| {
        observed.fetch_add(1, Ordering::Relaxed);
        crate::IrqEvent::HANDLED
    });
    let mut entry = IrqStateDesc::test_new(IrqDesc::from_virq(virq));
    let token = entry.install_shared_action(removed_owner.clone()).unwrap();
    entry.install_shared_action(survivor_owner.clone()).unwrap();
    let line = Arc::new(IrqLine::new(entry));
    let pin = kspin::NoPreemptIrqSave::new();
    let mut entry = line.lock();
    entry.begin_running_dispatch();
    let guard = IrqDispatchGuard::new_running(virq, line.clone());
    // SAFETY: entry locks the exact Line cloned into guard. Running ownership
    // above remains owned by guard until completion.
    let snapshot = unsafe { guard.snapshot_actions(&entry) };
    drop(entry);

    // No iterator has started yet: the detached head and its preserved next
    // link must remain valid solely because this original admission is alive.
    let retired = line.lock().withdraw_action(Some(token)).unwrap();
    assert_eq!(Arc::strong_count(&removed_owner), 2);
    assert!(!super::run_actions(virq, snapshot));
    assert_eq!(removed_calls.load(Ordering::Relaxed), 1);
    assert_eq!(survivor_calls.load(Ordering::Relaxed), 1);
    // SAFETY: this is the same retained Line and still-live admission used for
    // the first capture; withdrawal did not release running ownership.
    let snapshot = unsafe { guard.snapshot_actions(&line.lock()) };
    assert!(!super::run_actions(virq, snapshot));
    assert_eq!(removed_calls.load(Ordering::Relaxed), 1);
    assert_eq!(survivor_calls.load(Ordering::Relaxed), 2);
    let completion = line.lock().execution_idle_completion();
    let mut completions = 0;
    IrqDispatch {
        resolved_irq: Some(virq),
        guard: Some(guard),
    }
    .complete(|| {
        assert_eq!(Arc::strong_count(&removed_owner), 2);
        assert!(!completion.is_completed());
        completions += 1;
    });
    drop(pin);
    assert_eq!(completions, 1);
    assert!(completion.is_completed());
    // SAFETY: this private Line has one admission, now released after both
    // consuming traversals and original completion. No snapshot can still
    // access the detached node; future captures start at the surviving head.
    let removed = unsafe { retired.reclaim() };
    assert_eq!(removed.token(), token);
    assert_eq!(Arc::strong_count(&removed_owner), 2);
    drop(removed);
    assert_eq!(Arc::strong_count(&removed_owner), 1);
    drop(line);
    assert_eq!(Arc::strong_count(&survivor_owner), 1);
}

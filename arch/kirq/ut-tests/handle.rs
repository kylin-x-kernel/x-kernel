// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use kcpu_id_map::{KCpuMaskExt, LogicalCpuId};
use unittest::{assert, assert_eq, def_test};

use super::{IrqLineHandle, lookup_irq_line, next_irq_line};
use crate::{
    IrqLineRetirementObserver, IrqLineRetirementSubscription,
    Handler, IrqAffinityApplyError, IrqAffinityChip, IrqAffinityError, IrqAffinityPrepare,
    IrqDeferredAffinityChip, IrqDesc, IrqDescError, IrqEvent, IrqFlow, IrqRegisterOptions,
    LevelChip,
    action::IrqAction,
    runtime::{
        manager::{ManagementMode, publish_replaced_on_acquisition, with_retained_line},
        state::{IRQ_STATE, IrqLine, IrqStateDesc},
    },
};

static ACCEPT: IrqAffinityChip = IrqAffinityChip {
    apply: |_, eligible, _| Ok(*eligible),
};

static FAIL: IrqAffinityChip = IrqAffinityChip {
    apply: |_, _, _| Err(IrqAffinityApplyError::HardwareFailure),
};

static COUNTED: IrqAffinityChip = IrqAffinityChip {
    apply: |_, eligible, _| {
        APPLY_CALLS.fetch_add(1, Ordering::Relaxed);
        Ok(*eligible)
    },
};

static APPLY_CALLS: AtomicUsize = AtomicUsize::new(0);

static DEFERRED: IrqDeferredAffinityChip = IrqDeferredAffinityChip {
    accept: |_, _| Ok(()),
    prepare: |_, _, _| Ok(IrqAffinityPrepare::Pending(7)),
    finish: |_, _, _| Ok(None),
};

static LEVEL: LevelChip = LevelChip {
    mask_ack: |_| {},
    unmask: |_| {},
};

fn cpu_mask(cpu: usize) -> kcpu_id_map::KCpuMask {
    let mut mask = kcpu_id_map::KCpuMask::new();
    mask.set_logical(LogicalCpuId::new(cpu), true);
    mask
}

struct TestLine {
    virq: usize,
    line: Arc<IrqLine>,
}

impl TestLine {
    fn new(virq: usize) -> Self {
        let desc = IrqDesc::from_virq(virq);
        let line = Arc::new(IrqLine::new(IrqStateDesc::test_new(desc)));
        {
            let mut state = IRQ_STATE.lock();
            core::assert!(!state.descs.contains_key(&virq));
            core::assert!(state.descs.insert(virq, line.clone()).is_none());
        }
        Self { virq, line }
    }

    fn handle(&self) -> IrqLineHandle {
        lookup_irq_line(self.virq).unwrap().unwrap()
    }

    fn with_affinity_chip(&self) {
        self.with_chip(&ACCEPT);
    }

    fn with_chip(&self, chip: &'static IrqAffinityChip) {
        let mut entry = self.line.lock();
        entry.set_flow_for_tests(IrqFlow::Level(&LEVEL));
        entry.test_affinity_chip = Some(chip);
    }

    fn mark_retired(&self) {
        self.line.lock().set_retired_for_tests();
    }

    fn retire_and_notify(&self) {
        let slots = {
            let mut entry = self.line.lock();
            entry.set_retired_for_tests();
            entry.take_retirements()
        };
        crate::runtime::retirement::notify_retirements(self.virq, slots);
    }
}

impl Drop for TestLine {
    fn drop(&mut self) {
        let removed = {
            let mut state = IRQ_STATE.lock();
            if state
                .descs
                .get(&self.virq)
                .is_some_and(|line| Arc::ptr_eq(&line, &self.line))
            {
                state.remove_test_line(self.virq)
            } else {
                None
            }
        };
        drop(removed);
    }
}

#[def_test(serial)]
fn sparse_handles_follow_directory_and_maximum_key() {
    let first = usize::MAX - 2;
    let second = usize::MAX - 1;
    let _first = TestLine::new(first);
    let _second = TestLine::new(second);
    let _max = TestLine::new(usize::MAX);

    assert_eq!(lookup_irq_line(first).unwrap().unwrap().virq(), first);
    assert_eq!(
        lookup_irq_line(second + 1).unwrap().unwrap().virq(),
        usize::MAX
    );
    assert_eq!(next_irq_line(first).unwrap().unwrap().virq(), first);
    assert_eq!(next_irq_line(first + 1).unwrap().unwrap().virq(), second);
    assert_eq!(next_irq_line(second + 1).unwrap().unwrap().virq(), usize::MAX);
    assert_eq!(
        next_irq_line(usize::MAX).unwrap().unwrap().virq(),
        usize::MAX
    );
}

#[def_test(serial)]
fn stale_handle_never_targets_a_reused_virq() {
    let virq = 0x7e000030;
    let first = TestLine::new(virq);
    let stale = first.handle();
    let removed = { IRQ_STATE.lock().remove_test_line(virq) };
    drop(removed);

    let replacement = TestLine::new(virq);
    replacement.with_affinity_chip();
    assert_eq!(
        stale.irq_affinity(),
        Err(IrqAffinityError::Management(IrqDescError::UnknownIrq))
    );
    assert_eq!(
        stale.set_irq_affinity(&cpu_mask(0)),
        Err(IrqAffinityError::Management(IrqDescError::UnknownIrq))
    );
    assert!(replacement.handle().irq_affinity().is_ok());
    drop(first);
}

#[def_test]
fn management_admission_blocks_eligible_retirement_in_body() {
    let virq = 0x7e000040;
    let line = TestLine::new(virq);
    assert!(line.line.lock().is_unused());
    let result = with_retained_line(virq, &line.line, ManagementMode::Task, |_, admitted| {
        core::assert!(!admitted.lock().is_unused());
        core::assert!(!crate::runtime::manager::tests_irq::retire_unused_test_line(
            virq
        ));
        Ok(())
    });
    assert!(matches!(result, Ok(Some(()))));
    assert!(line.line.lock().is_unused());
}

#[def_test]
fn handle_query_and_setter_use_the_retained_line_transaction() {
    let virq = 0x7e000050;
    let line = TestLine::new(virq);
    line.with_affinity_chip();
    let handle = line.handle();
    let requested = cpu_mask(0);
    let changed = handle.set_irq_affinity(&requested).unwrap();
    assert_eq!(changed.requested, Some(requested));
    assert_eq!(handle.irq_affinity().unwrap(), changed);
    assert_eq!(handle.virq(), virq);
}

#[def_test(serial)]
fn enumeration_rejects_bh_disabled_task_context() {
    let _bh = crate::context::local_bh_disable();
    assert!(matches!(
        lookup_irq_line(0x7e000010),
        Err(IrqDescError::InvalidContext { .. })
    ));
    assert!(matches!(
        next_irq_line(0x7e000010),
        Err(IrqDescError::InvalidContext { .. })
    ));
}

#[def_test(serial)]
fn handle_setter_rejects_identity_mismatch_before_chip() {
    APPLY_CALLS.store(0, Ordering::Relaxed);
    let virq = 0x7e000060;
    let replacement = Arc::new(IrqLine::new(IrqStateDesc::test_new(IrqDesc::from_virq(
        virq,
    ))));
    assert!(
        replacement
            .lock()
            .install_regular_action(IrqAction::regular(Arc::new(|_| IrqEvent::HANDLED)))
    );
    let before = {
        let entry = replacement.lock();
        (
            entry.action_count(),
            entry.disable_depth_for_tests(),
            entry.affinity,
        )
    };
    let (original, _hook) = publish_replaced_on_acquisition(virq, &replacement);
    original.lock().set_flow_for_tests(IrqFlow::Level(&LEVEL));
    original.lock().test_affinity_chip = Some(&COUNTED);
    let handle = lookup_irq_line(virq).unwrap().unwrap();
    assert_eq!(
        handle.set_irq_affinity(&cpu_mask(0)),
        Err(IrqAffinityError::Management(IrqDescError::UnknownIrq))
    );
    assert_eq!(APPLY_CALLS.load(Ordering::Relaxed), 0);
    assert!(!original.lock().is_retired());
    let after = {
        let entry = replacement.lock();
        (
            entry.action_count(),
            entry.disable_depth_for_tests(),
            entry.affinity,
        )
    };
    assert_eq!(before, after);
    drop(handle);
    let removed = { IRQ_STATE.lock().remove_test_line(virq) };
    drop(removed);
}

#[def_test(serial)]
fn retired_handle_without_replacement_is_rejected() {
    APPLY_CALLS.store(0, Ordering::Relaxed);
    let virq = 0x7e000061;
    let line = TestLine::new(virq);
    line.with_chip(&COUNTED);
    let handle = line.handle();
    line.mark_retired();
    assert_eq!(
        handle.irq_affinity(),
        Err(IrqAffinityError::Management(IrqDescError::UnknownIrq))
    );
    assert_eq!(
        handle.set_irq_affinity(&cpu_mask(0)),
        Err(IrqAffinityError::Management(IrqDescError::UnknownIrq))
    );
    assert_eq!(APPLY_CALLS.load(Ordering::Relaxed), 0);
    assert!(lookup_irq_line(virq).unwrap().is_none());
}

#[def_test(serial)]
fn enumeration_skips_retired_candidates_and_terminal_maximum() {
    let first_key = usize::MAX - 4;
    let second_key = usize::MAX - 3;
    let first = TestLine::new(first_key);
    let second = TestLine::new(second_key);
    first.mark_retired();
    assert_eq!(
        next_irq_line(first_key).unwrap().unwrap().virq(),
        second_key
    );
    second.mark_retired();
    let terminal = TestLine::new(usize::MAX);
    assert_eq!(
        next_irq_line(first_key).unwrap().unwrap().virq(),
        usize::MAX
    );
    terminal.mark_retired();
    assert!(next_irq_line(usize::MAX).unwrap().is_none());
}

#[def_test(serial)]
fn handle_deferred_error_and_failure_paths_release_admission() {
    let deferred_virq = 0x7e000064;
    let deferred = TestLine::new(deferred_virq);
    {
        let mut entry = deferred.line.lock();
        entry.set_flow_for_tests(IrqFlow::Level(&LEVEL));
        entry.test_deferred_affinity = Some(&DEFERRED);
    }
    let deferred_handle = deferred.handle();
    let pending = deferred_handle.set_irq_affinity(&cpu_mask(0)).unwrap();
    assert!(pending.is_pending);
    assert_eq!(deferred_handle.irq_affinity().unwrap(), pending);
    let cleanup = with_retained_line(
        deferred_virq,
        &deferred.line,
        ManagementMode::Task,
        |_, _| Ok(()),
    );
    assert!(matches!(cleanup, Ok(Some(()))));

    let empty = TestLine::new(0x7e000065);
    empty.with_chip(&ACCEPT);
    assert_eq!(
        empty.handle().set_irq_affinity(&kcpu_id_map::KCpuMask::new()),
        Err(IrqAffinityError::NoReadyCpu)
    );
    let cleanup = with_retained_line(0x7e000065, &empty.line, ManagementMode::Task, |_, _| Ok(()));
    assert!(matches!(cleanup, Ok(Some(()))));
    assert!(empty.line.lock().is_unused());

    let failed = TestLine::new(0x7e000066);
    failed.with_chip(&FAIL);
    assert_eq!(
        failed.handle().set_irq_affinity(&cpu_mask(0)),
        Err(IrqAffinityError::HardwareFailure)
    );
    let cleanup = with_retained_line(0x7e000066, &failed.line, ManagementMode::Task, |_, _| Ok(()));
    assert!(matches!(cleanup, Ok(Some(()))));
    assert!(failed.line.lock().is_unused());
}

#[def_test(serial)]
fn unsupported_affinity_line_remains_enumerable() {
    let line = TestLine::new(0x7e000067);
    let handle = line.handle();
    assert_eq!(
        handle.irq_affinity(),
        Err(IrqAffinityError::Unsupported)
    );
    assert!(lookup_irq_line(0x7e000067).unwrap().is_some());
}

#[def_test(serial)]
fn handle_survives_last_action_and_drop_never_unregisters() {
    let virq = 0x7e000068;
    let line = TestLine::new(virq);
    let handle = line.handle();
    let handler: Handler = Arc::new(|_| IrqEvent::HANDLED);
    assert_eq!(crate::try_register_disabled(virq, handler.clone()), Ok(true));
    let removed = crate::try_free_irq(virq).unwrap().unwrap();
    assert!(Arc::ptr_eq(&removed, &handler));
    // The original handle still names the live, now-actionless Line; it must
    // report the Line's unsupported affinity, not a vanished identity.
    assert_eq!(handle.irq_affinity(), Err(IrqAffinityError::Unsupported));
    assert!(lookup_irq_line(virq).unwrap().is_some());
    drop(handle);

    let dropped = line.handle();
    let second: Handler = Arc::new(|_| IrqEvent::HANDLED);
    assert_eq!(crate::try_register_disabled(virq, second.clone()), Ok(true));
    drop(dropped);
    assert!(lookup_irq_line(virq).unwrap().is_some());
    let removed = crate::try_free_irq(virq).unwrap().unwrap();
    assert!(Arc::ptr_eq(&removed, &second));
    assert!(lookup_irq_line(virq).unwrap().is_some());
}

#[def_test(serial)]
fn enumeration_rejects_active_rcu_reader() {
    let result = krcu::with_read(|_| lookup_irq_line(0x7e000069));
    assert!(matches!(
        result,
        Err(IrqDescError::InvalidContext { .. })
    ));
}

#[def_test(serial)]
fn enumeration_rejects_hardirq_context() {
    let _irq = crate::context::test_support::ScopedHardIrqContext::enter();
    assert!(matches!(
        lookup_irq_line(0x7e000069),
        Err(IrqDescError::InvalidContext { .. })
    ));
}

#[def_test(serial)]
fn enumeration_rejects_serving_softirq_context() {
    // Softirq state alone does not disable preemption. Keep the synthetic
    // context non-preemptible so an unrelated kernel worker cannot run on this
    // CPU while the test's softirq depth is elevated.
    let _pin = kspin::NoPreemptIrqSave::new();
    let _softirq = crate::context::SoftIrqContextGuard::enter();
    assert!(matches!(
        next_irq_line(0x7e000069),
        Err(IrqDescError::InvalidContext { .. })
    ));
}

#[def_test(serial)]
fn enumeration_rejects_nmi_context() {
    let _pin = kspin::NoPreemptIrqSave::new();
    // SAFETY: the test pins CPU/IRQ exclusion and enters the synthetic NMI
    // watcher only for this non-scheduling context check.
    let result = unsafe {
        crate::context::test_support::with_nmi_context(|| lookup_irq_line(0x7e000069))
    };
    assert!(matches!(
        result,
        Err(IrqDescError::InvalidContext { .. })
    ));
}


#[def_test(serial)]
fn protected_regular_remains_readable_but_rejects_user_routing() {
    APPLY_CALLS.store(0, Ordering::Relaxed);
    let virq = 0x7e000070;
    let line = TestLine::new(virq);
    line.with_chip(&COUNTED);
    let handle = line.handle();
    let handler: Handler = Arc::new(|_| IrqEvent::HANDLED);
    assert_eq!(
        crate::try_register_with_options(
            virq,
            handler.clone(),
            IrqRegisterOptions::disabled().restrict_user_affinity(),
        ),
        Ok(true)
    );
    let before = handle.irq_affinity().unwrap();
    assert_eq!(
        handle.check_user_affinity_change(),
        Err(IrqAffinityError::UserRestricted)
    );
    assert_eq!(
        handle.set_user_affinity(&cpu_mask(0)),
        Err(IrqAffinityError::UserRestricted)
    );
    assert_eq!(APPLY_CALLS.load(Ordering::Relaxed), 0);
    assert_eq!(handle.irq_affinity().unwrap(), before);

    // The trusted kernel operation is deliberately policy-independent.
    assert!(handle.set_irq_affinity(&cpu_mask(0)).is_ok());
    assert_eq!(APPLY_CALLS.load(Ordering::Relaxed), 1);
    let removed = crate::try_free_irq(virq).unwrap().unwrap();
    assert!(Arc::ptr_eq(&removed, &handler));
    drop(removed);
}

#[def_test(serial)]
fn unprotected_user_path_and_policy_removal_are_independent() {
    let virq = 0x7e000071;
    let line = TestLine::new(virq);
    line.with_chip(&ACCEPT);
    let handle = line.handle();
    let handler: Handler = Arc::new(|_| IrqEvent::HANDLED);
    assert_eq!(
        crate::try_register_with_options(virq, handler, IrqRegisterOptions::disabled()),
        Ok(true)
    );
    assert_eq!(handle.check_user_affinity_change(), Ok(()));
    assert!(handle.set_user_affinity(&cpu_mask(0)).is_ok());
    let removed = crate::try_free_irq(virq).unwrap().unwrap();
    drop(removed);
    assert!(matches!(
        handle.check_user_affinity_change(),
        Err(IrqAffinityError::Management(IrqDescError::NoIrqAction { .. }))
    ));
}

#[def_test(serial)]
fn old_handle_cannot_bypass_new_protected_owner() {
    let virq = 0x7e000072;
    let line = TestLine::new(virq);
    line.with_chip(&ACCEPT);
    let old = line.handle();
    let handler: Handler = Arc::new(|_| IrqEvent::HANDLED);
    assert_eq!(
        crate::try_register_with_options(
            virq,
            handler,
            IrqRegisterOptions::disabled().restrict_user_affinity(),
        ),
        Ok(true)
    );
    assert_eq!(
        old.set_user_affinity(&cpu_mask(0)),
        Err(IrqAffinityError::UserRestricted)
    );
}

#[def_test(serial)]
fn protected_policy_follows_registration_and_removal() {
    let virq = 0x7e000073;
    let line = TestLine::new(virq);
    line.with_chip(&ACCEPT);
    assert!(matches!(
        line.handle().check_user_affinity_change(),
        Err(IrqAffinityError::Management(IrqDescError::NoIrqAction { .. }))
    ));
    let handler: Handler = Arc::new(|_| IrqEvent::HANDLED);
    assert_eq!(
        crate::try_register_with_options(
            virq,
            handler,
            IrqRegisterOptions::disabled().restrict_user_affinity(),
        ),
        Ok(true)
    );
    assert_eq!(
        line.handle().check_user_affinity_change(),
        Err(IrqAffinityError::UserRestricted)
    );
    let removed = crate::try_free_irq(virq).unwrap().unwrap();
    drop(removed);
    assert!(matches!(
        line.handle().check_user_affinity_change(),
        Err(IrqAffinityError::Management(IrqDescError::NoIrqAction { .. }))
    ));
    let second: Handler = Arc::new(|_| IrqEvent::HANDLED);
    assert_eq!(
        crate::try_register_with_options(virq, second, IrqRegisterOptions::disabled()),
        Ok(true)
    );
    assert_eq!(line.handle().check_user_affinity_change(), Ok(()));
    assert!(line.handle().set_user_affinity(&cpu_mask(0)).is_ok());
}

#[def_test(serial)]
fn user_policy_check_keeps_source_errors_first() {
    let virq = 0x7e000074;
    let line = TestLine::new(virq);
    line.with_chip(&ACCEPT);
    let handler: Handler = Arc::new(|_| IrqEvent::HANDLED);
    assert_eq!(
        crate::try_register_with_options(
            virq,
            handler,
            IrqRegisterOptions::disabled().restrict_user_affinity(),
        ),
        Ok(true)
    );
    line.line.lock().desc.affinity = crate::IrqAffinity::Cpu(0);
    assert_eq!(
        line.handle().check_user_affinity_change(),
        Err(IrqAffinityError::OwnerFixed)
    );
}

#[def_test(serial)]
fn protected_regular_rejects_shared_addition_without_relaxing_policy() {
    let virq = 0x7e000075;
    let line = TestLine::new(virq);
    line.with_chip(&ACCEPT);
    let handler: Handler = Arc::new(|_| IrqEvent::HANDLED);
    assert_eq!(
        crate::try_register_with_options(
            virq,
            handler,
            IrqRegisterOptions::disabled().restrict_user_affinity(),
        ),
        Ok(true)
    );
    let shared: Handler = Arc::new(|_| IrqEvent::HANDLED);
    assert_eq!(crate::try_register_shared(virq, shared), Ok(None));
    assert_eq!(
        line.handle().check_user_affinity_change(),
        Err(IrqAffinityError::UserRestricted)
    );
}

#[def_test(serial)]
fn unprotected_user_deferred_request_remains_pending() {
    let virq = 0x7e000076;
    let line = TestLine::new(virq);
    {
        let mut entry = line.line.lock();
        entry.set_flow_for_tests(IrqFlow::Level(&LEVEL));
        entry.test_deferred_affinity = Some(&DEFERRED);
    }
    let handler: Handler = Arc::new(|_| IrqEvent::HANDLED);
    assert_eq!(
        crate::try_register_with_options(virq, handler, IrqRegisterOptions::disabled()),
        Ok(true)
    );
    let handle = line.handle();
    assert_eq!(handle.check_user_affinity_change(), Ok(()));
    let snapshot = handle.set_user_affinity(&cpu_mask(0)).unwrap();
    assert!(snapshot.is_pending);
    assert_eq!(handle.irq_affinity().unwrap(), snapshot);
}


struct RetirementObserver {
    calls: Arc<AtomicUsize>,
    subscription: Option<Arc<kspin::SpinNoIrq<Option<IrqLineRetirementSubscription>>>>,
}

impl IrqLineRetirementObserver for RetirementObserver {
    fn on_line_retired(&self, _virq: crate::Virq) {
        self.calls.fetch_add(1, Ordering::AcqRel);
        if let Some(subscription) = &self.subscription {
            drop(subscription.lock().take());
        }
    }
}

#[def_test(serial)]
fn handle_identity_and_unchecked_affinity_snapshot() {
    let virq = 0x7e000071;
    let line = TestLine::new(virq);
    let first = line.handle();
    let second = line.handle();
    assert!(first.is_same_line(&second));
    assert!(first.is_current());
    assert_eq!(first.affinity_snapshot().unwrap(), Default::default());
    assert_eq!(
        first.irq_affinity(),
        Err(IrqAffinityError::Unsupported)
    );
    assert!(!first.has_action_history());
}

#[def_test(serial)]
fn retirement_subscription_notifies_and_cancels_independently() {
    let virq = 0x7e000072;
    let line = TestLine::new(virq);
    let calls = Arc::new(AtomicUsize::new(0));
    let observer: Arc<dyn IrqLineRetirementObserver> = Arc::new(RetirementObserver {
        calls: calls.clone(),
        subscription: None,
    });
    let first = line
        .handle()
        .subscribe_retirement(Arc::downgrade(&observer))
        .unwrap();
    let second = line
        .handle()
        .subscribe_retirement(Arc::downgrade(&observer))
        .unwrap();
    drop(first);
    line.retire_and_notify();
    assert_eq!(calls.load(Ordering::Acquire), 1);
    drop(second);
}

#[def_test(serial)]
fn retirement_subscription_cancel_after_retirement_does_not_touch_links() {
    let virq = 0x7e000073;
    let line = TestLine::new(virq);
    let calls = Arc::new(AtomicUsize::new(0));
    let observer: Arc<dyn IrqLineRetirementObserver> = Arc::new(RetirementObserver {
        calls: calls.clone(),
        subscription: None,
    });
    let subscription = line
        .handle()
        .subscribe_retirement(Arc::downgrade(&observer))
        .unwrap();
    line.retire_and_notify();
    assert_eq!(calls.load(Ordering::Acquire), 1);
    drop(subscription);
}

#[def_test(serial)]
fn retirement_callback_can_drop_its_own_subscription() {
    let virq = 0x7e000074;
    let line = TestLine::new(virq);
    let calls = Arc::new(AtomicUsize::new(0));
    let observer: Arc<dyn IrqLineRetirementObserver> = Arc::new(RetirementObserver {
        calls: calls.clone(),
        subscription: None,
    });
    let observer_slot = Arc::new(kspin::SpinNoIrq::new(None));
    let subscription = line
        .handle()
        .subscribe_retirement(Arc::downgrade(&observer))
        .unwrap();
    // The callback drops this slot; the subscription is not otherwise retained.
    let observer_with_slot: Arc<dyn IrqLineRetirementObserver> = Arc::new(RetirementObserver {
        calls: calls.clone(),
        subscription: Some(observer_slot.clone()),
    });
    let callback_subscription = line
        .handle()
        .subscribe_retirement(Arc::downgrade(&observer_with_slot))
        .unwrap();
    *observer_slot.lock() = Some(callback_subscription);
    line.retire_and_notify();
    assert_eq!(calls.load(Ordering::Acquire), 2);
    drop(subscription);
}

#[def_test(serial)]
fn handle_identity_distinguishes_same_number_successor() {
    let virq = 0x7e000075;
    let first = TestLine::new(virq);
    let first_handle = first.handle();
    first.mark_retired();
    drop(first);

    let second = TestLine::new(virq);
    let second_handle = second.handle();
    assert!(!first_handle.is_same_line(&second_handle));
    assert!(!first_handle.is_current());
    assert!(second_handle.is_current());
    assert!(!first_handle.has_action_history());
}

#[def_test(serial)]
fn retirement_cancel_after_detach_before_notify_is_safe() {
    let virq = 0x7e000077;
    let line = TestLine::new(virq);
    let calls = Arc::new(AtomicUsize::new(0));
    let observer: Arc<dyn IrqLineRetirementObserver> = Arc::new(RetirementObserver {
        calls: calls.clone(),
        subscription: None,
    });
    let subscription = line
        .handle()
        .subscribe_retirement(Arc::downgrade(&observer))
        .unwrap();
    let slots = {
        let mut entry = line.line.lock();
        entry.set_retired_for_tests();
        entry.take_retirements()
    };
    drop(subscription);
    crate::runtime::retirement::notify_retirements(virq, slots);
    assert_eq!(calls.load(Ordering::Acquire), 1);
}

#[def_test(serial)]
fn retired_and_stale_handles_cannot_subscribe() {
    let virq = 0x7e000078;
    let calls = Arc::new(AtomicUsize::new(0));
    let observer: Arc<dyn IrqLineRetirementObserver> = Arc::new(RetirementObserver {
        calls: calls.clone(),
        subscription: None,
    });

    let line = TestLine::new(virq);
    let stale = line.handle();
    line.mark_retired();
    assert!(matches!(
        stale.subscribe_retirement(Arc::downgrade(&observer)),
        Err(IrqDescError::UnknownIrq)
    ));
    drop(line);

    let _successor = TestLine::new(virq);
    assert!(matches!(
        stale.subscribe_retirement(Arc::downgrade(&observer)),
        Err(IrqDescError::UnknownIrq)
    ));
    assert_eq!(calls.load(Ordering::Acquire), 0);
}

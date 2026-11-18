// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use kcpu_id_map::{KCpuMaskExt, LogicalCpuId};
use unittest::{assert, assert_eq, def_test};

use super::*;
use crate::{IrqAffinityChip, IrqDesc, IrqEvent, runtime::state::IrqLine};

static ACCEPT_MASKED: IrqAffinityChip = IrqAffinityChip {
    apply: |_, eligible, delivery| {
        core::assert_eq!(delivery, IrqAffinityDelivery::KeepMasked);
        Ok(*eligible)
    },
};
static ACCEPT_ENABLED: IrqAffinityChip = IrqAffinityChip {
    apply: |_, eligible, delivery| {
        core::assert_eq!(delivery, IrqAffinityDelivery::Preserve);
        Ok(*eligible)
    },
};
static SELECT_FIRST: IrqAffinityChip = IrqAffinityChip {
    apply: |_, eligible, _| {
        core::assert!(eligible.get_logical(LogicalCpuId::new(0)));
        Ok(cpu_mask(0))
    },
};
static REJECT: IrqAffinityChip = IrqAffinityChip {
    apply: |_, _, _| Err(IrqAffinityApplyError::Rejected),
};
static FAIL: IrqAffinityChip = IrqAffinityChip {
    apply: |_, _, _| Err(IrqAffinityApplyError::HardwareFailure),
};
static EMPTY_SUCCESS: IrqAffinityChip = IrqAffinityChip {
    apply: |_, _, _| Ok(KCpuMask::new()),
};
static WRONG_TARGET: IrqAffinityChip = IrqAffinityChip {
    apply: |_, _, _| Ok(cpu_mask(1)),
};
static MUST_NOT_APPLY: IrqAffinityChip = IrqAffinityChip {
    apply: unexpected_apply,
};
static DEFERRED: crate::IrqDeferredAffinityChip = crate::IrqDeferredAffinityChip {
    accept: |_, _| Ok(()),
    prepare: |_, _, _| Ok(crate::IrqAffinityPrepare::Pending(7)),
    finish: |_, token, _| {
        core::assert_eq!(token, 7);
        Ok(Some(kiface_cpu_api::CpuInterruptReadinessIf::ready_cpus()))
    },
};

static DEFERRED_STILL_PENDING: crate::IrqDeferredAffinityChip = crate::IrqDeferredAffinityChip {
    accept: |_, _| Ok(()),
    prepare: |_, _, _| Ok(crate::IrqAffinityPrepare::Pending(9)),
    finish: |_, token, _| {
        core::assert_eq!(token, 9);
        Ok(None)
    },
};

static DEFERRED_COMPLETE_CANDIDATES: crate::IrqDeferredAffinityChip =
    crate::IrqDeferredAffinityChip {
        accept: |_, _| Ok(()),
        prepare: |_, candidates, _| Ok(crate::IrqAffinityPrepare::Complete(*candidates)),
        finish: |_, _, _| panic!("completed edge transaction must not finish"),
    };

static DEFERRED_FAIL: crate::IrqDeferredAffinityChip = crate::IrqDeferredAffinityChip {
    accept: |_, _| Ok(()),
    prepare: |_, _, _| Err(IrqAffinityApplyError::HardwareFailure),
    finish: |_, _, _| Err(IrqAffinityApplyError::HardwareFailure),
};

static DEFERRED_EDGE: crate::IrqDeferredAffinityChip = crate::IrqDeferredAffinityChip {
    accept: |_, _| Ok(()),
    prepare: |_, _, _| {
        Ok(crate::IrqAffinityPrepare::Complete(
            kiface_cpu_api::CpuInterruptReadinessIf::ready_cpus(),
        ))
    },
    finish: |_, _, _| Err(IrqAffinityApplyError::Rejected),
};


#[expect(
    clippy::panic,
    reason = "CG-021-no-panic-on-reachable-failure: test scenario: pre-write rejection must not \
              invoke the chip"
)]
fn unexpected_apply(
    _: IrqDesc,
    _: &KCpuMask,
    _: IrqAffinityDelivery,
) -> Result<KCpuMask, IrqAffinityApplyError> {
    panic!("rejected affinity request reached hardware callback");
}

fn cpu_mask(cpu: usize) -> KCpuMask {
    let mut mask = KCpuMask::new();
    mask.set_logical(LogicalCpuId::new(cpu), true);
    mask
}

fn entry(chip: &'static IrqAffinityChip) -> IrqStateDesc {
    let mut entry = IrqStateDesc::test_new(IrqDesc::from_virq(0x6f20));
    entry.test_affinity_chip = Some(chip);
    entry
}

#[def_test]
fn requested_policy_is_distinct_from_confirmed_ready_destinations() {
    let mut entry = entry(&ACCEPT_MASKED);
    let ready = cpu_mask(0);
    let mut requested = ready;
    if kbuild_config::NR_CPUS > 1 {
        requested |= cpu_mask(1);
    }
    let before = entry.desc;
    let snapshot = apply_locked(&mut entry, &requested, ready).unwrap();
    assert_eq!(snapshot.requested, Some(requested));
    assert_eq!(snapshot.effective, Some(ready));
    assert!(!snapshot.is_pending);
    assert!(!snapshot.has_failed_programming);
    assert_eq!(entry.desc, before);
    assert!(!entry.is_enabled_for_tests());
    entry.test_affinity_chip = Some(&SELECT_FIRST);
    let selected = apply_locked(&mut entry, &requested, requested).unwrap();
    assert_eq!(selected.requested, Some(requested));
    assert_eq!(selected.effective, Some(ready));
}

#[def_test]
fn routing_preserves_shared_tokens_owners_and_handler_delivery() {
    let mut entry = entry(&ACCEPT_ENABLED);
    let first: crate::Handler = Arc::new(|_| IrqEvent::HANDLED);
    let second: crate::Handler = Arc::new(|_| IrqEvent::HANDLED);
    let first_token = entry.install_shared_action(first.clone()).unwrap();
    let second_token = entry.install_shared_action(second.clone()).unwrap();
    let plan = entry.prepare_auto_enable();
    entry.commit_platform_plan(plan);
    let depth = entry.disable_depth_for_tests();
    let generation = entry.generation_for_tests();
    let mask = cpu_mask(0);
    assert!(apply_locked(&mut entry, &mask, mask).is_ok());
    assert_eq!(entry.action_count(), 2);
    assert_eq!(entry.disable_depth_for_tests(), depth);
    assert_eq!(entry.generation_for_tests(), generation);
    assert!(entry.can_run_actions());
    entry.begin_running_dispatch();
    let mut tokens = [None; 2];
    let mut count = 0;
    let mut all_handled = true;
    // SAFETY: exclusive private state supplies Line exclusion. Running ownership
    // and all nodes survive until this consuming traversal finishes.
    unsafe { entry.snapshot_actions() }.with_actions(|action| {
        if count < tokens.len() {
            tokens[count] = Some(action.token());
        }
        count += 1;
        all_handled &= action.run_primary(0).handled();
    });
    drop(entry.finish_running_dispatch());
    assert_eq!(count, 2);
    assert_eq!(tokens, [Some(first_token), Some(second_token)]);
    assert!(all_handled);
    assert_eq!(Arc::strong_count(&first), 2);
    assert_eq!(Arc::strong_count(&second), 2);
    // No short locks are held, and the completed private traversal is the only
    // reader. Linked action destruction therefore runs in the owning test task.
    drop(entry);
    assert_eq!(Arc::strong_count(&first), 1);
    assert_eq!(Arc::strong_count(&second), 1);
}

#[def_test]
fn empty_ready_intersection_and_controller_rejection_preserve_policy() {
    let mut entry = entry(&ACCEPT_MASKED);
    let mask = cpu_mask(0);
    let previous = apply_locked(&mut entry, &mask, mask).unwrap();
    entry.test_affinity_chip = Some(&MUST_NOT_APPLY);
    assert_eq!(
        apply_locked(&mut entry, &mask, KCpuMask::new()),
        Err(IrqAffinityError::NoReadyCpu)
    );
    assert_eq!(
        apply_locked(&mut entry, &KCpuMask::new(), mask),
        Err(IrqAffinityError::NoReadyCpu)
    );
    assert_eq!(entry.affinity, previous);
    entry.test_affinity_chip = Some(&REJECT);
    let request = if kbuild_config::NR_CPUS > 1 {
        cpu_mask(1)
    } else {
        mask
    };
    assert_eq!(
        apply_locked(&mut entry, &request, request),
        Err(IrqAffinityError::Rejected)
    );
    assert_eq!(entry.affinity, previous);
}

#[def_test]
fn hardware_failure_closes_admission_until_disabled_recovery_and_explicit_enable() {
    let mut entry = entry(&ACCEPT_ENABLED);
    let owner: crate::Handler = Arc::new(|_| IrqEvent::HANDLED);
    let token = entry.install_shared_action(owner.clone()).unwrap();
    let plan = entry.prepare_auto_enable();
    entry.commit_platform_plan(plan);
    let mask = cpu_mask(0);
    let previous = apply_locked(&mut entry, &mask, mask).unwrap();
    assert!(entry.can_run_actions());
    entry.test_affinity_chip = Some(&FAIL);
    assert_eq!(
        apply_locked(&mut entry, &mask, mask),
        Err(IrqAffinityError::HardwareFailure)
    );
    assert_eq!(entry.affinity.requested, previous.requested);
    assert_eq!(entry.affinity.effective, None);
    assert!(entry.affinity.has_failed_programming);
    assert!(!entry.can_run_actions());
    assert!(!entry.can_install_regular_action());
    assert!(!entry.can_install_shared_action());
    assert!(!entry.can_install_nmi_action());
    assert!(entry.require_affinity_ready().is_err());
    assert_eq!(entry.action_count(), 1);
    assert_eq!(Arc::strong_count(&owner), 2);
    entry.test_affinity_chip = Some(&MUST_NOT_APPLY);
    assert_eq!(
        apply_locked(&mut entry, &mask, mask),
        Err(IrqAffinityError::RecoveryRequiresDisable)
    );

    let plan = entry.prepare_disable_irq_nosync();
    entry.commit_platform_plan(plan);
    let depth = entry.disable_depth_for_tests();
    assert!(depth > 0);
    entry.test_affinity_chip = Some(&ACCEPT_MASKED);
    assert_eq!(apply_locked(&mut entry, &mask, mask).unwrap(), previous);
    assert_eq!(entry.disable_depth_for_tests(), depth);
    assert!(!entry.is_enabled_for_tests());
    assert!(!entry.can_run_actions());
    assert!(entry.require_affinity_ready().is_ok());
    assert!(entry.can_install_shared_action());
    let plan = entry.prepare_enable_irq();
    entry.commit_platform_plan(plan);
    assert!(entry.can_run_actions());
    assert_eq!(entry.disable_depth_for_tests(), 0);
    let retired = entry.withdraw_action(Some(token)).unwrap();
    // SAFETY: this private fixture has no dispatch, snapshot or concurrent reader.
    let removed = unsafe { retired.reclaim() };
    assert!(Arc::ptr_eq(removed.primary(), &owner));
    drop(removed);
    assert_eq!(Arc::strong_count(&owner), 1);
}

#[def_test]
fn malformed_success_faults_without_inventing_an_effective_route() {
    let mask = cpu_mask(0);
    for chip in [&EMPTY_SUCCESS, &WRONG_TARGET] {
        if core::ptr::eq(chip, &WRONG_TARGET) && kbuild_config::NR_CPUS == 1 {
            continue;
        }
        let mut entry = entry(&ACCEPT_MASKED);
        let previous = apply_locked(&mut entry, &mask, mask).unwrap();
        entry.test_affinity_chip = Some(chip);
        assert_eq!(
            apply_locked(&mut entry, &mask, mask),
            Err(IrqAffinityError::HardwareFailure)
        );
        assert_eq!(entry.affinity.requested, previous.requested);
        assert_eq!(entry.affinity.effective, None);
        assert!(entry.affinity.has_failed_programming);
        // Empty membership makes these checks independent of occupancy rejection.
        assert!(!entry.can_install_regular_action());
        assert!(!entry.can_install_shared_action());
        assert!(!entry.can_install_nmi_action());
        entry.test_affinity_chip = Some(&REJECT);
        let fault = entry.affinity;
        assert_eq!(
            apply_locked(&mut entry, &mask, mask),
            Err(IrqAffinityError::Rejected)
        );
        assert_eq!(entry.affinity, fault);
    }
}

#[def_test]
fn source_owned_and_unbound_routes_reject_before_chip_access() {
    let mask = cpu_mask(0);
    for flags in [IrqFlags::MSI, IrqFlags::PER_CPU] {
        let mut entry = entry(&MUST_NOT_APPLY);
        entry.desc = entry.desc.with_flags(flags);
        assert_eq!(
            apply_locked(&mut entry, &mask, mask),
            Err(IrqAffinityError::Unsupported)
        );
    }
    let mut percpu = entry(&MUST_NOT_APPLY);
    percpu.set_flow_for_tests(IrqFlow::PerCpu);
    assert_eq!(
        apply_locked(&mut percpu, &mask, mask),
        Err(IrqAffinityError::Unsupported)
    );
    let mut fixed = entry(&MUST_NOT_APPLY);
    fixed.desc = fixed.desc.with_affinity(IrqAffinity::Cpu(0));
    assert_eq!(
        apply_locked(&mut fixed, &mask, mask),
        Err(IrqAffinityError::OwnerFixed)
    );
    let mut nmi = entry(&MUST_NOT_APPLY);
    nmi.set_flow_for_tests(IrqFlow::PerCpu);
    nmi.install_nmi_action();
    assert_eq!(
        apply_locked(&mut nmi, &mask, mask),
        Err(IrqAffinityError::Unsupported)
    );
    let mut unbound = IrqStateDesc::new(IrqDesc::from_virq(0x6f21));
    unbound.test_affinity_chip = Some(&MUST_NOT_APPLY);
    assert_eq!(
        apply_locked(&mut unbound, &mask, mask),
        Err(IrqAffinityError::Unsupported)
    );
}

// Serial tests own this otherwise vacant directory slot. Retain the exact Line
// and destroy it only after releasing directory and Line locks.
struct QueryFixture {
    virq: Virq,
    line: Arc<IrqLine>,
}

impl Drop for QueryFixture {
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
fn enable_runs_masked_deferred_recovery_before_unmask() {
    let virq = crate::DYNAMIC_VIRQ_BASE + 0x6f40;
    assert!(!IRQ_STATE.lock().descs.contains_key(&virq));
    let ready = kiface_cpu_api::CpuInterruptReadinessIf::ready_cpus();
    assert!(!ready.is_empty());
    let mut state = IrqStateDesc::test_new(IrqDesc::from_virq(virq));
    state.test_deferred_affinity = Some(&DEFERRED);
    assert!(
        state
            .install_shared_action(Arc::new(|_| IrqEvent::HANDLED))
            .is_some()
    );
    state.affinity.requested = Some(ready);
    state.affinity.effective = None;
    state.affinity.has_failed_programming = true;
    state.affinity.is_pending = true;
    let plan = state.prepare_register_disabled();
    state.commit_platform_plan(plan);
    let fixture = QueryFixture { virq, line: Arc::new(IrqLine::new(state)) };
    IRQ_STATE.lock().descs.insert(virq, fixture.line.clone());

    crate::try_enable_irq(virq).unwrap();
    let entry = fixture.line.lock();
    assert!(entry.is_enabled_for_tests());
    assert_eq!(entry.affinity.effective, Some(ready));
    assert!(!entry.affinity.has_failed_programming);
    assert!(!entry.affinity.is_pending);
}

#[def_test(serial)]
fn failed_pending_line_registers_disabled_after_masked_recovery() {
    let virq = crate::DYNAMIC_VIRQ_BASE + 0x6f42;
    let ready = kiface_cpu_api::CpuInterruptReadinessIf::ready_cpus();
    let mut state = IrqStateDesc::test_new(IrqDesc::from_virq(virq));
    state.desc = IrqDesc::from_virq(virq);
    state.test_deferred_affinity = Some(&DEFERRED);
    state.affinity.requested = Some(ready);
    state.affinity.effective = None;
    state.affinity.has_failed_programming = true;
    state.affinity.is_pending = true;
    let fixture = QueryFixture { virq, line: Arc::new(IrqLine::new(state)) };
    IRQ_STATE.lock().descs.insert(virq, fixture.line.clone());

    assert!(crate::try_register_disabled(virq, Arc::new(|_| IrqEvent::HANDLED)).unwrap());
    let entry = fixture.line.lock();
    assert_eq!(entry.action_count(), 1);
    assert_eq!(entry.affinity.effective, Some(ready));
    assert!(!entry.affinity.has_failed_programming);
    assert!(!entry.affinity.is_pending);
}

#[def_test(serial)]
fn failed_pending_line_registers_shared_after_masked_recovery() {
    let virq = crate::DYNAMIC_VIRQ_BASE + 0x6f43;
    let ready = kiface_cpu_api::CpuInterruptReadinessIf::ready_cpus();
    let mut state = IrqStateDesc::test_new(IrqDesc::from_virq(virq));
    state.desc = IrqDesc::from_virq(virq);
    state.test_deferred_affinity = Some(&DEFERRED);
    state.affinity.requested = Some(ready);
    state.affinity.effective = None;
    state.affinity.has_failed_programming = true;
    state.affinity.is_pending = true;
    let fixture = QueryFixture { virq, line: Arc::new(IrqLine::new(state)) };
    IRQ_STATE.lock().descs.insert(virq, fixture.line.clone());

    assert!(
        crate::try_register_shared(virq, Arc::new(|_| IrqEvent::HANDLED))
            .unwrap()
            .is_some()
    );
    let entry = fixture.line.lock();
    assert_eq!(entry.action_count(), 1);
    assert_eq!(entry.affinity.effective, Some(ready));
    assert!(!entry.affinity.has_failed_programming);
    assert!(!entry.affinity.is_pending);
}

#[def_test(serial)]
fn failed_recovery_registration_publishes_no_action() {
    let virq = crate::DYNAMIC_VIRQ_BASE + 0x6f44;
    let ready = kiface_cpu_api::CpuInterruptReadinessIf::ready_cpus();
    let mut state = IrqStateDesc::test_new(IrqDesc::from_virq(virq));
    state.desc = IrqDesc::from_virq(virq);
    state.test_deferred_affinity = Some(&DEFERRED_FAIL);
    state.affinity.requested = Some(ready);
    state.affinity.effective = None;
    state.affinity.has_failed_programming = true;
    state.affinity.is_pending = true;
    let fixture = QueryFixture { virq, line: Arc::new(IrqLine::new(state)) };
    IRQ_STATE.lock().descs.insert(virq, fixture.line.clone());

    assert!(crate::try_register_disabled(virq, Arc::new(|_| IrqEvent::HANDLED)).is_err());
    let entry = fixture.line.lock();
    assert_eq!(entry.action_count(), 0);
    assert!(entry.affinity.has_failed_programming);
    assert!(!entry.affinity.is_pending);
}

#[def_test(serial)]
fn shared_recovery_failure_keeps_existing_action_and_returns_no_token() {
    let virq = crate::DYNAMIC_VIRQ_BASE + 0x6f45;
    let ready = kiface_cpu_api::CpuInterruptReadinessIf::ready_cpus();
    let mut state = IrqStateDesc::test_new(IrqDesc::from_virq(virq));
    state.desc = IrqDesc::from_virq(virq);
    state.test_deferred_affinity = Some(&DEFERRED_FAIL);
    assert!(
        state
            .install_shared_action(Arc::new(|_| IrqEvent::HANDLED))
            .is_some()
    );
    let plan = state.prepare_register_disabled();
    state.commit_platform_plan(plan);
    state.affinity.requested = Some(ready);
    state.affinity.effective = None;
    state.affinity.has_failed_programming = true;
    state.affinity.is_pending = true;
    let fixture = QueryFixture { virq, line: Arc::new(IrqLine::new(state)) };
    IRQ_STATE.lock().descs.insert(virq, fixture.line.clone());

    assert!(crate::try_register_shared(virq, Arc::new(|_| IrqEvent::HANDLED)).is_err());
    let entry = fixture.line.lock();
    assert_eq!(entry.action_count(), 1);
    assert!(entry.affinity.has_failed_programming);
    assert!(!entry.affinity.is_pending);
}

#[def_test(serial)]
fn last_action_cancels_pending_and_reregistration_does_not_revive_it() {
    let virq = crate::DYNAMIC_VIRQ_BASE + 0x6f41;
    assert!(!IRQ_STATE.lock().descs.contains_key(&virq));
    let ready = kiface_cpu_api::CpuInterruptReadinessIf::ready_cpus();
    let mut state = IrqStateDesc::test_new(IrqDesc::from_virq(virq));
    state.test_deferred_affinity = Some(&DEFERRED);
    let token = state
        .install_shared_action(Arc::new(|_| IrqEvent::HANDLED))
        .unwrap();
    let plan = state.prepare_auto_enable();
    state.commit_platform_plan(plan);
    assert!(apply_locked(&mut state, &ready, ready).unwrap().is_pending);
    let fixture = QueryFixture { virq, line: Arc::new(IrqLine::new(state)) };
    IRQ_STATE.lock().descs.insert(virq, fixture.line.clone());

    drop(crate::try_free_irq_action(virq, token).unwrap());
    {
        let entry = fixture.line.lock();
        assert!(!entry.affinity.is_pending);
        assert_eq!(entry.affinity.requested, entry.affinity.effective);
    }
    assert!(crate::try_register_disabled(virq, Arc::new(|_| IrqEvent::HANDLED)).unwrap());
    assert!(!fixture.line.lock().affinity.is_pending);
    drop(crate::try_free_irq(virq).unwrap());
}

#[def_test(serial)]
fn actionless_route_fault_preserves_binding_for_disabled_recovery() {
    let virq = crate::DYNAMIC_VIRQ_BASE + 0x6f21;
    assert!(!IRQ_STATE.lock().descs.contains_key(&virq));
    let mut state = entry(&ACCEPT_MASKED);
    let desc = IrqDesc::from_virq(virq);
    state.desc = desc;
    let mask = cpu_mask(0);
    let confirmed = apply_locked(&mut state, &mask, mask).unwrap();
    state.test_affinity_chip = Some(&FAIL);
    assert_eq!(
        apply_locked(&mut state, &mask, mask),
        Err(IrqAffinityError::HardwareFailure)
    );
    let fault = state.affinity;
    let generation = state.generation_for_tests();
    assert!(!state.has_actions());
    assert!(!state.is_enabled_for_tests());
    assert_eq!(state.disable_depth_for_tests(), 0);
    let fixture = QueryFixture {
        virq,
        line: Arc::new(IrqLine::new(state)),
    };
    IRQ_STATE.lock().descs.insert(virq, fixture.line.clone());
    assert_eq!(
        crate::try_map(desc.with_affinity(IrqAffinity::Cpu(0))),
        Err(IrqDescError::UnsupportedFlow { hwirq: desc.hwirq })
    );
    assert_eq!(irq_affinity(virq).unwrap(), fault);
    let (preserved_desc, preserved_generation, capability_preserved) = {
        let entry = fixture.line.lock();
        (
            entry.desc,
            entry.generation_for_tests(),
            core::ptr::eq(entry.affinity_chip().unwrap(), &FAIL),
        )
    };
    assert_eq!(preserved_desc, desc);
    assert_eq!(preserved_generation, generation);
    assert!(capability_preserved);
    // Switch only the injected hardware response, not the binding or policy.
    // Never-enabled Lines already satisfy the recovery suppression prerequisite.
    let repaired = {
        let mut entry = fixture.line.lock();
        entry.test_affinity_chip = Some(&ACCEPT_MASKED);
        apply_locked(&mut entry, &mask, mask)
    };
    assert_eq!(repaired.unwrap(), confirmed);
    let (depth, is_enabled, desc_after) = {
        let entry = fixture.line.lock();
        (
            entry.disable_depth_for_tests(),
            entry.is_enabled_for_tests(),
            entry.desc,
        )
    };
    assert_eq!(depth, 0);
    assert!(!is_enabled);
    assert_eq!(desc_after, desc);
    assert_eq!(irq_affinity(virq).unwrap(), confirmed);
}

#[def_test(serial)]
fn query_does_not_fabricate_initial_route_and_setter_checks_context() {
    let virq = crate::DYNAMIC_VIRQ_BASE + 0x6f20;
    assert!(!IRQ_STATE.lock().descs.contains_key(&virq));
    assert_eq!(
        irq_affinity(virq),
        Err(IrqAffinityError::Management(IrqDescError::UnknownIrq))
    );
    assert_eq!(
        set_irq_affinity(virq, &cpu_mask(0)),
        Err(IrqAffinityError::Management(IrqDescError::UnknownIrq))
    );
    assert!(!IRQ_STATE.lock().descs.contains_key(&virq));
    let mut state = entry(&MUST_NOT_APPLY);
    state.desc = IrqDesc::from_virq(virq);
    let fixture = QueryFixture {
        virq,
        line: Arc::new(IrqLine::new(state)),
    };
    IRQ_STATE.lock().descs.insert(virq, fixture.line.clone());
    assert_eq!(irq_affinity(virq).unwrap(), IrqAffinitySnapshot::default());
    let rejected = {
        let _guard = kspin::NoPreemptIrqSave::new();
        set_irq_affinity(virq, &cpu_mask(0))
    };
    assert!(matches!(
        rejected,
        Err(IrqAffinityError::Management(
            IrqDescError::InvalidContext { .. }
        ))
    ));
    assert_eq!(irq_affinity(virq).unwrap(), IrqAffinitySnapshot::default());
}

#[def_test(serial)]
fn public_enable_and_registration_cannot_bypass_routing_fault() {
    let virq = crate::DYNAMIC_VIRQ_BASE + 0x6f22;
    assert!(!IRQ_STATE.lock().descs.contains_key(&virq));
    let mut state = entry(&FAIL);
    state.desc = IrqDesc::from_virq(virq);
    let token = state.install_shared_action(Arc::new(|_| IrqEvent::HANDLED)).unwrap();
    let plan = state.prepare_auto_enable();
    state.commit_platform_plan(plan);
    let fixture = QueryFixture { virq, line: Arc::new(IrqLine::new(state)) };
    IRQ_STATE.lock().descs.insert(virq, fixture.line.clone());
    let ready = kiface_cpu_api::CpuInterruptReadinessIf::ready_cpus();
    assert!(!ready.is_empty());
    assert_eq!(set_irq_affinity(virq, &ready), Err(IrqAffinityError::HardwareFailure));
    let blocked = IrqDescError::ConfigurationFailed { hwirq: virq };
    assert_eq!(crate::try_enable_irq(virq), Err(blocked));
    assert_eq!(crate::try_enable(virq, true), Err(blocked));
    assert!(crate::try_register_shared(virq, Arc::new(|_| IrqEvent::HANDLED)).unwrap().is_none());
    assert_eq!(fixture.line.lock().action_count(), 1);
    // Last-action withdrawal must remain available even when the route is unknown.
    drop(crate::try_free_irq_action(virq, token).unwrap());
    assert_eq!(fixture.line.lock().action_count(), 0);
    assert!(!crate::try_register_disabled(virq, Arc::new(|_| IrqEvent::HANDLED)).unwrap());
    fixture.line.lock().test_affinity_chip = Some(&ACCEPT_MASKED);
    assert!(set_irq_affinity(virq, &ready).is_ok());
    assert!(!fixture.line.lock().can_run_actions());
    assert!(crate::try_register_disabled(virq, Arc::new(|_| IrqEvent::HANDLED)).unwrap());
    assert!(!fixture.line.lock().can_run_actions());
    crate::try_enable_irq(virq).unwrap();
    assert!(fixture.line.lock().can_run_actions());
    drop(crate::try_free_irq(virq).unwrap());
}

#[def_test]
fn deferred_request_stays_pending_until_completion_finishes() {
    let mut entry = IrqStateDesc::test_new(IrqDesc::from_virq(0x6f30));
    entry.test_deferred_affinity = Some(&DEFERRED);
    let ready = kiface_cpu_api::CpuInterruptReadinessIf::ready_cpus();
    let accepted = apply_locked(&mut entry, &ready, ready).unwrap();
    assert_eq!(accepted.requested, Some(ready));
    assert_eq!(accepted.effective, None);
    assert!(accepted.is_pending);
    assert!(!accepted.has_failed_programming);

    let token = super::prepare_deferred_affinity(&mut entry).unwrap();
    assert_eq!(token, 7);
    assert!(entry.affinity.is_pending);
    super::finish_deferred_affinity(&mut entry, token);
    assert_eq!(entry.affinity.requested, Some(ready));
    assert_eq!(entry.affinity.effective, Some(ready));
    assert!(!entry.affinity.is_pending);
    assert!(!entry.affinity.has_failed_programming);
}

#[def_test]
fn a_new_deferred_request_replaces_the_pending_target() {
    let mut entry = IrqStateDesc::test_new(IrqDesc::from_virq(0x6f33));
    entry.test_deferred_affinity = Some(&DEFERRED_COMPLETE_CANDIDATES);
    let ready = kiface_cpu_api::CpuInterruptReadinessIf::ready_cpus();
    let mut cpus = ready.iter_logical();
    let first_cpu = cpus.next().expect("at least one ready CPU");
    let second_cpu = cpus.next().unwrap_or(first_cpu);
    let first = cpu_mask(first_cpu.as_usize());
    let second = cpu_mask(second_cpu.as_usize());
    assert!(apply_locked(&mut entry, &first, ready).unwrap().is_pending);
    let snapshot = apply_locked(&mut entry, &second, ready).unwrap();
    assert_eq!(snapshot.requested, Some(second));
    assert!(snapshot.is_pending);
    assert_eq!(snapshot.effective, None);

    assert_eq!(super::prepare_deferred_affinity(&mut entry), None);
    assert_eq!(entry.affinity.effective, Some(second));
    assert!(!entry.affinity.is_pending);
    if second != first {
        assert_ne!(entry.affinity.effective, Some(first));
    }
}

#[def_test]
fn edge_deferred_transaction_completes_before_eoi() {
    let mut entry = IrqStateDesc::test_new(IrqDesc::from_virq(0x6f31));
    entry.test_deferred_affinity = Some(&DEFERRED_EDGE);
    let ready = kiface_cpu_api::CpuInterruptReadinessIf::ready_cpus();
    let accepted = apply_locked(&mut entry, &ready, ready).unwrap();
    assert!(accepted.is_pending);
    assert_eq!(super::prepare_deferred_affinity(&mut entry), None);
    assert_eq!(entry.affinity.effective, Some(ready));
    assert!(!entry.affinity.is_pending);
}

#[def_test]
fn first_activation_applies_pending_without_an_old_eoi() {
    let mut entry = IrqStateDesc::test_new(IrqDesc::from_virq(0x6f34));
    entry.test_deferred_affinity = Some(&DEFERRED);
    let ready = kiface_cpu_api::CpuInterruptReadinessIf::ready_cpus();
    assert!(apply_locked(&mut entry, &ready, ready).unwrap().is_pending);
    super::apply_deferred_before_activation(&mut entry).unwrap();
    assert_eq!(entry.affinity.effective, Some(ready));
    assert!(!entry.affinity.is_pending);
}

#[def_test]
fn cancelling_pending_reconciles_requested_with_effective() {
    let mut entry = IrqStateDesc::test_new(IrqDesc::from_virq(0x6f32));
    entry.test_deferred_affinity = Some(&DEFERRED);
    let ready = kiface_cpu_api::CpuInterruptReadinessIf::ready_cpus();
    entry.affinity.requested = Some(ready);
    entry.affinity.effective = Some(ready);
    entry.affinity.is_pending = true;
    entry.cancel_pending_affinity();
    assert_eq!(entry.affinity.requested, Some(ready));
    assert_eq!(entry.affinity.effective, Some(ready));
    assert!(!entry.affinity.is_pending);
}

#[def_test]
fn masked_recovery_activation_clears_failure_without_an_old_eoi() {
    let mut entry = IrqStateDesc::test_new(IrqDesc::from_virq(0x6f35));
    entry.test_deferred_affinity = Some(&DEFERRED);
    let ready = kiface_cpu_api::CpuInterruptReadinessIf::ready_cpus();
    entry.affinity.requested = Some(ready);
    entry.affinity.effective = None;
    entry.affinity.has_failed_programming = true;
    entry.affinity.is_pending = true;
    assert!(entry.require_affinity_activation_ready().is_ok());
    super::apply_deferred_before_activation(&mut entry).unwrap();
    assert_eq!(entry.affinity.effective, Some(ready));
    assert!(!entry.affinity.has_failed_programming);
    assert!(!entry.affinity.is_pending);
}

#[def_test]
fn confirmed_old_route_can_activate_while_finish_stays_pending() {
    let mut entry = IrqStateDesc::test_new(IrqDesc::from_virq(0x6f36));
    entry.test_deferred_affinity = Some(&DEFERRED_STILL_PENDING);
    let ready = cpu_mask(0);
    entry.affinity.requested = Some(ready);
    entry.affinity.effective = Some(ready);
    entry.affinity.is_pending = true;
    super::apply_deferred_before_activation(&mut entry).unwrap();
    assert_eq!(entry.affinity.effective, Some(ready));
    assert!(entry.affinity.is_pending);
    assert!(!entry.affinity.has_failed_programming);
}

#[def_test]
fn initial_policy_prepares_without_publishing_and_retains_explicit_policy() {
    let mut entry = IrqStateDesc::test_new(IrqDesc::from_virq(0x6f40));
    assert_eq!(
        entry.prepare_initial_affinity_policy_with(None),
        Err(crate::IrqDescError::NotReady)
    );
    let policy = entry.prepare_initial_affinity_policy();
    match kcpu_id_map::present_mask() {
        Some(present) => {
            assert_eq!(policy, Ok(Some(present)));
            entry.commit_initial_affinity_policy(policy.unwrap());
            assert_eq!(entry.affinity.requested, Some(present));
            assert_eq!(entry.affinity.effective, None);
        }
        None => assert_eq!(policy, Err(crate::IrqDescError::NotReady)),
    }

    let mut fixed = IrqStateDesc::test_new(
        IrqDesc::from_virq(0x6f41).with_affinity(crate::IrqAffinity::Cpu(0)),
    );
    assert_eq!(
        fixed.prepare_initial_affinity_policy(),
        Ok(Some(cpu_mask(0)))
    );
    fixed.commit_initial_affinity_policy(Some(cpu_mask(0)));
    assert_eq!(fixed.affinity.requested, Some(cpu_mask(0)));

    let explicit = cpu_mask(0);
    let mut retained = IrqStateDesc::test_new(IrqDesc::from_virq(0x6f42));
    retained.affinity.requested = Some(explicit);
    assert_eq!(retained.prepare_initial_affinity_policy(), Ok(None));
}

#[def_test]
fn action_history_is_set_by_publication_and_survives_withdrawal() {
    let mut entry = IrqStateDesc::test_new(IrqDesc::from_virq(0x6f43));
    assert!(!entry.has_action_history());
    let handler: crate::Handler = Arc::new(|_| IrqEvent::HANDLED);
    let token = entry.install_shared_action(handler).unwrap();
    assert!(entry.has_action_history());
    let _retired = entry.withdraw_action(Some(token)).unwrap();
    assert!(entry.has_action_history());
}

#[def_test]
fn cancelling_absent_pending_does_not_clear_requested_policy() {
    let mut entry = IrqStateDesc::test_new(IrqDesc::from_virq(0x6f44));
    let requested = cpu_mask(0);
    entry.affinity.requested = Some(requested);
    entry.affinity.effective = None;
    entry.affinity.is_pending = false;
    entry.cancel_pending_affinity();
    assert_eq!(entry.affinity.requested, Some(requested));
    assert_eq!(entry.affinity.effective, None);
    assert!(!entry.affinity.is_pending);
}

#[def_test(serial)]
fn first_registration_commits_default_policy_after_action_publication() {
    let virq = crate::DYNAMIC_VIRQ_BASE + 0x6f45;
    if let Some(present) = kcpu_id_map::present_mask() {
        let state = IrqStateDesc::test_new(IrqDesc::from_virq(virq));
        let fixture = QueryFixture { virq, line: Arc::new(IrqLine::new(state)) };
        IRQ_STATE.lock().descs.insert(virq, fixture.line.clone());

        assert!(crate::try_register_disabled(virq, Arc::new(|_| IrqEvent::HANDLED)).unwrap());
        {
            let entry = fixture.line.lock();
            assert_eq!(entry.affinity.requested, Some(present));
            assert_eq!(entry.affinity.effective, None);
            assert!(entry.has_action_history());
        }
        drop(crate::try_free_irq(virq).unwrap());
        assert!(fixture.line.lock().has_action_history());
    }
}

#[def_test]
fn fixed_owner_cpu_must_be_representable_before_policy_commit() {
    for cpu in [kbuild_config::NR_CPUS, usize::MAX] {
        let entry = IrqStateDesc::test_new(
            IrqDesc::from_virq(0x6f46).with_affinity(crate::IrqAffinity::Cpu(cpu)),
        );
        assert_eq!(
            entry.prepare_initial_affinity_policy(),
            Err(crate::IrqDescError::InvalidCpu { cpu })
        );
        assert_eq!(entry.affinity.requested, None);
        assert_eq!(entry.affinity.effective, None);
        assert!(!entry.has_action_history());
    }
}

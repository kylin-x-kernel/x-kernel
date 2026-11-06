// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, def_test};

use super::{IrqDesc, LocalControl};

#[cfg(target_arch = "aarch64")]
mod percpu_registration {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicUsize, Ordering};

    use unittest::{assert, assert_eq, def_test};

    use crate::{
        Handler, IrqDesc, IrqDescError, IrqEvent, IrqFlow,
        state::{IRQ_STATE, IrqLine, IrqStateDesc},
    };

    struct Fixture {
        virq: usize,
        line: Arc<IrqLine>,
    }

    impl Fixture {
        fn new(hwirq: usize, configured: bool) -> Self {
            let virq = crate::DYNAMIC_VIRQ_BASE + 0x5fc8;
            let desc = IrqDesc::new(hwirq, crate::IrqTrigger::LevelHigh).with_virq(virq);
            let mut entry = IrqStateDesc::new(desc);
            entry.set_flow_for_tests(crate::IntrManagerIf::select_flow(desc).unwrap());
            if configured {
                // Private software slots only: skip controller configuration
                // on every possible calling CPU without touching real SGI15.
                let generation = entry.generation_for_tests();
                for (cpu, slot) in entry.local.iter_mut().enumerate() {
                    let plan = slot.reconfigure(desc, generation, cpu);
                    slot.commit(plan, generation);
                }
            }
            let line = Arc::new(IrqLine::new(entry));
            let mut registry = IRQ_STATE.lock();
            core::assert!(!registry.descs.contains_key(&virq));
            registry.descs.insert(virq, line.clone());
            Self { virq, line }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let removed = {
                let mut registry = IRQ_STATE.lock();
                if registry
                    .descs
                    .get(&self.virq)
                    .is_some_and(|line| Arc::ptr_eq(&line, &self.line))
                {
                    registry.remove_test_line(self.virq)
                } else {
                    None
                }
            };
            drop(removed);
        }
    }

    fn local_state(line: &IrqLine) -> [(bool, bool, Option<u64>); kbuild_config::NR_CPUS] {
        let entry = line.lock();
        core::array::from_fn(|cpu| {
            let slot = &entry.local[cpu];
            (
                slot.is_enabled,
                slot.has_failed_configuration,
                slot.configured_generation,
            )
        })
    }

    #[def_test(serial)]
    fn generic_registration_rejects_vacant_and_occupied_resolved_percpu() {
        for occupied in [false, true] {
            let fixture = Fixture::new(15, occupied);
            let owner: Handler = Arc::new(|_| IrqEvent::HANDLED);
            if occupied {
                assert_eq!(
                    crate::try_register_percpu(fixture.virq, owner.clone()),
                    Ok(true)
                );
            }
            let offered: Handler = Arc::new(|_| panic!("rejected handler was published"));
            let before = local_state(&fixture.line);
            let desc = fixture.line.lock().desc;
            // Raw SGI identity resolves to PerCpu without a PER_CPU flag.
            // On the vacant fixture, accidental configuration also marks a
            // failed slot: LevelHigh SGI15 is rejected before any MMIO.
            let expected = Err(IrqDescError::UnsupportedFlow { hwirq: 15 });
            assert_eq!(crate::try_register(fixture.virq, offered.clone()), expected);
            assert_eq!(
                crate::try_register_disabled(fixture.virq, offered.clone()),
                expected
            );
            assert_eq!(
                crate::try_register_startup(
                    fixture.virq,
                    offered.clone(),
                    crate::IrqRegisterOptions::disabled()
                ),
                expected
            );
            assert_eq!(
                crate::try_register_startup(
                    fixture.virq,
                    offered.clone(),
                    crate::IrqRegisterOptions::auto_enable()
                ),
                expected
            );
            assert_eq!(
                crate::try_register_shared(fixture.virq, offered.clone())
                    .map(|token| token.is_some()),
                expected
            );
            assert_eq!(local_state(&fixture.line), before);
            assert_eq!(fixture.line.lock().desc, desc);
            assert_eq!(fixture.line.lock().action_count(), usize::from(occupied));
            assert_eq!(Arc::strong_count(&offered), 1);
            if occupied {
                // SAFETY: this serial fixture never enables or dispatches its
                // synthetic Line. No claim, snapshot or completion is outstanding,
                // and no other owner can restart it during removal.
                let removed = unsafe { crate::try_free_percpu_irq(fixture.virq) }
                    .unwrap()
                    .unwrap();
                assert!(Arc::ptr_eq(&removed, &owner));
                drop(removed);
            }
            assert_eq!(Arc::strong_count(&owner), 1);
        }
    }

    #[def_test(serial)]
    fn generic_retirement_rejects_percpu_without_changing_local_state_or_owner() {
        let fixture = Fixture::new(15, true);
        let owner: Handler = Arc::new(|_| IrqEvent::HANDLED);
        assert_eq!(
            crate::try_register_percpu(fixture.virq, owner.clone()),
            Ok(true)
        );
        for enabled in [false, true] {
            for slot in &mut fixture.line.lock().local {
                slot.is_enabled = enabled;
            }
            let before = local_state(&fixture.line);
            let expected = Err(IrqDescError::UnsupportedFlow { hwirq: 15 });
            assert_eq!(
                crate::try_free_irq(fixture.virq).map(|owner| owner.is_some()),
                expected
            );
            assert_eq!(
                crate::try_free_irq_action(fixture.virq, crate::IrqActionToken::new(1))
                    .map(|owner| owner.is_some()),
                expected
            );
            assert_eq!(crate::try_synchronize_irq(fixture.virq), expected);
            assert_eq!(crate::try_disable_irq(fixture.virq), expected);
            assert_eq!(local_state(&fixture.line), before);
            assert_eq!(fixture.line.lock().action_count(), 1);
            assert!(!fixture.line.lock().is_running);
            assert!(!fixture.line.lock().is_teardown_in_progress());
            assert_eq!(Arc::strong_count(&owner), 2);
        }
        for slot in &mut fixture.line.lock().local {
            slot.is_enabled = false;
        }
        // Do not run local control against the real SGI15: non-nesting disable
        // now always applies the hardware mask, even for a disabled software slot.
        // SAFETY: enabled slots above were software-only; this serial fixture
        // never dispatches or activates the synthetic Line. All slots are now
        // disabled, with no outstanding snapshot/completion or concurrent restart.
        let removed = unsafe { crate::try_free_percpu_irq(fixture.virq) }
            .unwrap()
            .unwrap();
        assert!(Arc::ptr_eq(&removed, &owner));
        drop(removed);
        assert_eq!(fixture.line.lock().action_count(), 0);
        assert_eq!(Arc::strong_count(&owner), 1);
    }

    #[def_test(serial)]
    fn percpu_public_registration_is_disabled_and_occupied_preserves_owner() {
        let fixture = Fixture::new(15, true);
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let handler: Handler = Arc::new(move |_| {
            observed.fetch_add(1, Ordering::Relaxed);
            IrqEvent::HANDLED
        });
        assert_eq!(
            crate::try_register_percpu(fixture.virq, handler.clone()),
            Ok(true)
        );
        let offered: Handler = Arc::new(|_| panic!("occupied registration replaced the owner"));
        let desc = fixture.line.lock().desc;
        assert_eq!(
            crate::try_register_percpu(desc.with_flags(crate::IrqFlags::SHARED), offered.clone()),
            Ok(false)
        );
        assert_eq!(fixture.line.lock().desc, desc);
        assert_eq!(Arc::strong_count(&offered), 1);
        assert_eq!(Arc::strong_count(&handler), 2);
        let pin = kspin::NoPreemptIrqSave::new();
        assert!(!fixture.line.lock().has_enabled_local_cpu());
        let mut completions = 0;
        crate::dispatch::dispatch_claim(crate::IrqRef::Virq(fixture.virq), || completions += 1);
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        {
            let mut entry = fixture.line.lock();
            // Exercise explicit local enable accounting without enabling a
            // live controller source for this synthetic descriptor.
            let plan = entry.prepare_enable_irq();
            assert_eq!(plan.enable, Some(true));
            entry.commit_platform_plan(plan);
        }
        crate::dispatch::dispatch_claim(crate::IrqRef::Virq(fixture.virq), || completions += 1);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(completions, 2);
        drop(pin);
        drop(fixture);
        assert_eq!(Arc::strong_count(&handler), 1);
    }

    #[def_test(serial)]
    fn percpu_registration_rejects_enabled_local_and_remote_slots_before_configuration() {
        let fixture = Fixture::new(15, false);
        let handler: Handler = Arc::new(|_| IrqEvent::HANDLED);
        let _pin = kspin::NoPreemptIrqSave::new();
        let current = super::super::current_cpu();
        for cpu in 0..kbuild_config::NR_CPUS {
            {
                let mut entry = fixture.line.lock();
                entry.local[cpu].is_enabled = true;
                assert_eq!(entry.local[current].is_enabled, cpu == current);
            }
            // LevelHigh SGI15 would fail checked configuration. Ok(false)
            // therefore proves enabled-slot rejection preceded configuration.
            assert_eq!(
                crate::try_register_percpu_startup(fixture.virq, handler.clone()),
                Ok(false)
            );
            let mut entry = fixture.line.lock();
            assert_eq!(entry.action_count(), 0);
            assert_eq!(entry.flow, Some(IrqFlow::PerCpu));
            assert!(entry.local[cpu].is_enabled);
            assert!(
                entry
                    .local
                    .iter()
                    .all(|slot| slot.configured_generation.is_none())
            );
            assert_eq!(Arc::strong_count(&handler), 1);
            entry.local[cpu].is_enabled = false;
        }
    }

    #[def_test(serial)]
    fn percpu_registration_checks_flow_and_configuration_failure() {
        let handler: Handler = Arc::new(|_| IrqEvent::HANDLED);
        {
            let desc = IrqDesc::from_hwirq(0x5fc8).with_virq(crate::DYNAMIC_VIRQ_BASE + 0x5fc8);
            // Wrong-flow admission is independent of real controller support:
            // bind this source explicitly to the no-op ordinary test chip.
            let fixture = crate::runtime::manager::tests_irq::SyntheticLine::new(desc);
            assert_eq!(
                crate::try_register_percpu(fixture.virq(), handler.clone()),
                Err(IrqDescError::UnsupportedFlow { hwirq: 0x5fc8 })
            );
            let line = IRQ_STATE.lock().descs.get(&fixture.virq()).unwrap();
            assert_eq!(line.lock().action_count(), 0);
            assert_eq!(Arc::strong_count(&handler), 1);
        }
        let fixture = Fixture::new(15, false);
        // The GIC checked-config path rejects level-triggered SGIs before MMIO.
        assert_eq!(
            crate::try_register_percpu(fixture.virq, handler.clone()),
            Err(IrqDescError::UnsupportedFlow { hwirq: 15 })
        );
        assert_eq!(fixture.line.lock().action_count(), 0);
        assert!(!fixture.line.lock().has_enabled_local_cpu());
        assert_eq!(Arc::strong_count(&handler), 1);
    }

    #[def_test(serial)]
    fn percpu_registration_checks_missing_descriptor_and_startup_context() {
        let missing = crate::DYNAMIC_VIRQ_BASE + 0x5fc9;
        assert!(crate::descriptor(missing).is_none());
        let handler: Handler = Arc::new(|_| IrqEvent::HANDLED);
        assert_eq!(
            crate::try_register_percpu(missing, handler.clone()),
            Err(IrqDescError::UnknownIrq)
        );
        assert_eq!(
            crate::try_register_percpu_startup(missing, handler.clone()),
            Err(IrqDescError::UnknownIrq)
        );
        let fixture = Fixture::new(15, true);
        let _pin = kspin::NoPreemptIrqSave::new();
        assert!(matches!(
            crate::try_register_percpu(fixture.virq, handler.clone()),
            Err(IrqDescError::InvalidContext { .. })
        ));
        {
            let _irq = crate::context::HardIrqContextGuard::enter();
            assert!(matches!(
                crate::try_register_percpu_startup(fixture.virq, handler.clone()),
                Err(IrqDescError::InvalidContext { .. })
            ));
        }
        assert_eq!(fixture.line.lock().action_count(), 0);
        assert_eq!(
            crate::try_register_percpu_startup(fixture.virq, handler.clone()),
            Ok(true)
        );
        assert!(!fixture.line.lock().has_enabled_local_cpu());
        assert!(!karch::local_irq_enabled());
    }
}

// Stateless backend fixtures keep parallel tests independent.
static PREPARE_FAILURE: crate::PerCpuNmiChip = crate::PerCpuNmiChip {
    prepare: |desc| Err(crate::IrqDescError::ConfigurationFailed { hwirq: desc.hwirq }),
    teardown: |_| Ok(()),
};

static TEARDOWN_FAILURE: crate::PerCpuNmiChip = crate::PerCpuNmiChip {
    prepare: |_| Ok(()),
    teardown: |desc| Err(crate::IrqDescError::ConfigurationFailed { hwirq: desc.hwirq }),
};

static READY_CHIP: crate::PerCpuNmiChip = crate::PerCpuNmiChip {
    prepare: |_| Ok(()),
    teardown: |_| Ok(()),
};

#[def_test]
fn failed_nmi_preparation_retains_cleanup_and_rejects_backend_replacement() {
    let desc = IrqDesc::from_virq(0x5fb3);
    let mut slot = LocalControl::new();
    assert!(slot.prepare_nmi(desc, &PREPARE_FAILURE).is_err());
    assert!(slot.has_nmi_preparation());
    assert!(slot.has_failed_nmi_preparation());
    assert!(slot.prepare_nmi(desc, &READY_CHIP).is_err());
    assert!(slot.teardown_nmi(desc).is_ok());
    assert!(!slot.has_nmi_preparation());
    assert!(slot.prepare_nmi(desc, &READY_CHIP).is_ok());
    assert!(!slot.has_failed_nmi_preparation());
}

#[def_test]
fn enabled_nmi_cannot_teardown_and_failed_teardown_retains_obligation() {
    let desc = IrqDesc::from_virq(0x5fb4);
    let mut slot = LocalControl::new();
    assert!(slot.prepare_nmi(desc, &TEARDOWN_FAILURE).is_ok());
    let plan = slot.enable(desc, 1, 0);
    slot.commit(plan, 1);
    assert!(slot.prepare_nmi(desc, &TEARDOWN_FAILURE).is_ok());
    assert!(slot.teardown_nmi(desc).is_err());
    assert!(!slot.has_failed_nmi_preparation());
    let plan = slot.disable(desc, 0);
    slot.commit(plan, 1);
    assert!(slot.teardown_nmi(desc).is_err());
    assert!(slot.has_nmi_preparation());
    assert!(slot.has_failed_nmi_preparation());
}

#[def_test]
fn repeated_local_disable_needs_one_enable_and_keeps_other_cpu_running() {
    let desc = IrqDesc::from_virq(0x5f80);
    // Explicit CPU identities exercise the protocol even in a UP test build.
    let mut slots = [LocalControl::new(); 2];
    for (cpu, slot) in slots.iter_mut().enumerate() {
        // Enabling one CPU must not admit the next CPU before its local setup.
        assert!(!slot.can_run());
        let plan = slot.enable(desc, 7, cpu);
        assert_eq!(plan.cpu, Some(cpu));
        assert!(plan.configure);
        assert_eq!(plan.enable, Some(true));
        slot.commit(plan, 7);
    }
    for _ in 0..2 {
        let plan = slots[0].disable(desc, 0);
        assert_eq!(plan.enable, Some(false));
        slots[0].commit(plan, 7);
    }
    assert!(!slots[0].can_run());
    assert!(slots[1].can_run());
    let final_enable = slots[0].enable(desc, 7, 0);
    assert!(!final_enable.configure);
    assert_eq!(final_enable.enable, Some(true));
    slots[0].commit(final_enable, 7);
    assert!(slots[0].can_run());
    assert!(slots[1].can_run());
}

#[def_test]
fn local_gate_plans_commit_only_after_controller_success() {
    let desc = IrqDesc::from_virq(0x5f86);
    let mut slot = LocalControl::new();
    let enable = slot.enable(desc, 1, 0);
    assert!(enable.configure);
    assert_eq!(enable.enable, Some(true));
    assert!(!slot.can_run());
    assert_eq!(slot.configured_generation, None);

    // An abandoned configuration plan cannot publish readiness. Retrying still
    // requests both configuration and unmask before software admission commits.
    let retry = slot.enable(desc, 1, 0);
    assert!(retry.configure);
    slot.commit(retry, 1);
    assert!(slot.can_run());
    let repeated = slot.enable(desc, 1, 0);
    assert!(!repeated.configure);
    assert_eq!(repeated.enable, Some(true));
    slot.commit(repeated, 1);

    let disable = slot.disable(desc, 0);
    assert!(slot.can_run());
    assert_eq!(disable.enable, Some(false));
    slot.commit(disable, 1);
    assert!(!slot.can_run());
    assert_eq!(slot.configured_generation, Some(1));
}

#[def_test]
fn disabled_registration_masks_only_the_enabled_local_slot() {
    let desc = IrqDesc::from_virq(0x5f85);
    let mut slots = [LocalControl::new(); 2];
    for (cpu, slot) in slots.iter_mut().enumerate() {
        let plan = slot.enable(desc, 1, cpu);
        slot.commit(plan, 1);
    }
    let disabled = slots[0].register_disabled(desc, 0);
    assert_eq!(disabled.cpu, Some(0));
    assert_eq!(disabled.enable, Some(false));
    slots[0].commit(disabled, 1);
    assert!(!slots[0].is_enabled);
    assert!(!slots[0].can_run());
    assert!(slots[1].can_run());

    let repeated = slots[0].disable(desc, 0);
    assert_eq!(repeated.enable, Some(false));
    slots[0].commit(repeated, 1);
    let registered_again = slots[0].register_disabled(desc, 0);
    assert_eq!(registered_again.enable, None);
    slots[0].commit(registered_again, 1);
    let enabled = slots[0].enable(desc, 1, 0);
    assert_eq!(enabled.enable, Some(true));
    slots[0].commit(enabled, 1);
    assert!(slots[0].can_run());
    assert!(slots[1].can_run());
}

#[def_test]
fn failed_local_configuration_does_not_poison_another_cpu() {
    let desc = IrqDesc::from_virq(0x5f81);
    let mut slots = [LocalControl::new(); 2];
    for (cpu, slot) in slots.iter_mut().enumerate() {
        let plan = slot.enable(desc, 1, cpu);
        slot.commit(plan, 1);
    }
    slots[0].has_failed_configuration = true;
    assert!(!slots[0].can_run());
    assert!(slots[1].can_run());
    let retry = slots[0].enable(desc, 1, 0);
    assert!(retry.configure);
    assert_eq!(retry.enable, Some(true));
    slots[0].commit(retry, 1);
    assert!(slots[0].can_run());
    assert!(!slots[1].reconfigure(desc, 1, 1).configure);
    assert!(slots[1].reconfigure(desc, 2, 1).configure);
}

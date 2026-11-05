// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use unittest::{assert, assert_eq, def_test};

use super::{
    Handler, IRQ_STATE, IrqAction, IrqDesc, NMI_TABLE, register_nmi, register_nmi_startup,
    unregister_nmi,
};
use crate::{
    GIC_ROOT_DOMAIN, IrqActionToken, IrqController, IrqDescError, IrqEvent, IrqSource,
    runtime::rcu::with_nmi_context,
    state::{IrqLine, IrqStateDesc},
};

fn is_invalid_context<T>(result: Result<T, IrqDescError>) -> bool {
    matches!(result, Err(IrqDescError::InvalidContext { .. }))
}

struct ScopedFailedNmi {
    virq: usize,
    hwirq: usize,
}

impl Drop for ScopedFailedNmi {
    fn drop(&mut self) {
        let table_owner = { NMI_TABLE.lock().remove(&self.hwirq) };
        let line = { IRQ_STATE.lock().remove_test_line(self.virq) };
        drop(table_owner);
        drop(line);
    }
}

#[def_test(serial)]
fn startup_nmi_registration_reuses_prepared_mapping() {
    let virq = crate::DYNAMIC_VIRQ_BASE + 0x5f7b;
    let hwirq = 0x5f7b;
    let desc = IrqDesc::from_hwirq(hwirq)
        .with_virq(virq)
        .with_domain(crate::domain::TEST_DOMAIN);
    assert!(crate::descriptor(virq).is_none());
    let cleanup = ScopedFailedNmi { virq, hwirq };
    assert_eq!(crate::try_map(desc), Ok(virq));
    let prepared = IRQ_STATE.lock().descs.get(&virq).unwrap();
    assert_eq!(prepared.lock().action_count(), 0);
    let handler: Handler = Arc::new(|_| IrqEvent::HANDLED);
    // The synthetic source has no hardware producer. Only configuration is
    // injected; mapping, startup admission and both action publications are real.
    let irq_guard = kspin::IrqSave::new();
    assert!(super::register_nmi_with_mode(
        desc,
        handler.clone(),
        super::ManagementMode::Startup,
        |_| Ok(()),
    ));
    assert!(!karch::local_irq_enabled());
    drop(irq_guard);
    let installed = IRQ_STATE.lock().descs.get(&virq).unwrap();
    assert!(Arc::ptr_eq(&prepared, &installed));
    assert_eq!(installed.lock().action_count(), 1);
    assert!(
        NMI_TABLE
            .lock()
            .get(&hwirq)
            .is_some_and(|owner| Arc::ptr_eq(owner, &handler))
    );
    drop(installed);
    drop(prepared);
    drop(cleanup);
    assert_eq!(Arc::strong_count(&handler), 1);
}

#[def_test(serial)]
fn task_and_startup_nmi_reject_missing_mapping_before_configuration() {
    let virq = crate::DYNAMIC_VIRQ_BASE + 0x5fac;
    let hwirq = 0x5fac;
    let desc = IrqDesc::from_hwirq(hwirq)
        .with_virq(virq)
        .with_domain(crate::domain::TEST_DOMAIN);
    assert!(crate::descriptor(virq).is_none());
    assert!(!NMI_TABLE.lock().contains_key(&hwirq));
    let _cleanup = ScopedFailedNmi { virq, hwirq };
    let handler: Handler = Arc::new(|_| IrqEvent::HANDLED);
    let mut has_configured = false;
    let task_registered =
        super::register_nmi_with_mode(desc, handler.clone(), super::ManagementMode::Task, |_| {
            has_configured = true;
            Ok(())
        });
    assert!(!task_registered && !has_configured);
    let irq_before = karch::local_irq_enabled();
    let guard = kspin::IrqSave::new();
    let registered = super::register_nmi_with_mode(
        desc,
        handler.clone(),
        super::ManagementMode::Startup,
        |_| {
            has_configured = true;
            Ok(())
        },
    );
    assert!(!registered && !has_configured);
    assert!(!karch::local_irq_enabled());
    drop(guard);
    assert_eq!(karch::local_irq_enabled(), irq_before);
    assert!(crate::descriptor(virq).is_none());
    assert!(!NMI_TABLE.lock().contains_key(&hwirq));
    assert!(
        IRQ_STATE
            .lock()
            .translated_hwirq(crate::domain::TEST_DOMAIN, hwirq)
            .is_none()
    );
    assert_eq!(Arc::strong_count(&handler), 1);
}

#[def_test(serial)]
fn failed_nmi_configuration_publishes_neither_owner_and_preserves_irq_mask() {
    let virq = crate::DYNAMIC_VIRQ_BASE + 0x5f7a;
    let hwirq = 0x5f7a;
    let domain = crate::domain::TEST_DOMAIN;
    let desc = IrqDesc::from_hwirq(hwirq)
        .with_virq(virq)
        .with_domain(domain);
    let handler: Handler = Arc::new(|_| IrqEvent::HANDLED);
    for is_masked in [false, true] {
        assert!(crate::descriptor(virq).is_none());
        assert!(!NMI_TABLE.lock().contains_key(&hwirq));
        assert!(IRQ_STATE.lock().translated_hwirq(domain, hwirq).is_none());
        let cleanup = ScopedFailedNmi { virq, hwirq };
        assert_eq!(crate::try_map(desc), Ok(virq));
        let irq_before = karch::local_irq_enabled();
        let irq_guard = is_masked.then(kspin::IrqSave::new);
        let expected_irq = karch::local_irq_enabled();
        let mode = if is_masked {
            super::ManagementMode::Startup
        } else {
            super::ManagementMode::Task
        };
        let mut observed = None;
        let registered = super::register_nmi_with_mode(desc, handler.clone(), mode, |stored| {
            observed = Some((
                stored,
                Arc::strong_count(&handler),
                karch::local_irq_enabled(),
            ));
            Err(IrqDescError::ConfigurationFailed { hwirq })
        });
        assert!(!registered);
        assert_eq!(
            observed,
            Some((desc.with_flags(crate::IrqFlags::PER_CPU), 2, false))
        );
        assert_eq!(karch::local_irq_enabled(), expected_irq);
        drop(irq_guard);
        assert_eq!(karch::local_irq_enabled(), irq_before);
        assert!(!NMI_TABLE.lock().contains_key(&hwirq));
        assert_eq!(Arc::strong_count(&handler), 1);
        assert_eq!(IRQ_STATE.lock().translated_hwirq(domain, hwirq), Some(virq));
        {
            let line = IRQ_STATE.lock().descs.get(&virq).unwrap();
            let entry = line.lock();
            assert_eq!(entry.desc, desc.with_flags(crate::IrqFlags::PER_CPU));
            assert_eq!(entry.action_count(), 0);
            assert_eq!(entry.configured_generation_for_tests(), None);
            assert!(!entry.is_enabled_for_tests());
            assert_eq!(entry.disable_depth_for_tests(), 0);
        }
        let mut has_reconfigured = false;
        assert!(super::register_nmi_with_mode(
            desc,
            handler.clone(),
            super::ManagementMode::Task,
            |_| {
                has_reconfigured = true;
                Ok(())
            },
        ));
        assert!(has_reconfigured);
        {
            let line = IRQ_STATE.lock().descs.get(&virq).unwrap();
            let entry = line.lock();
            assert_eq!(
                entry.configured_generation_for_tests(),
                Some(entry.generation_for_tests())
            );
            assert!(!entry.prepare_reconfigure_if_stale().configure);
            assert_eq!(entry.action_count(), 1);
        }
        assert!(
            NMI_TABLE
                .lock()
                .get(&hwirq)
                .is_some_and(|owner| Arc::ptr_eq(owner, &handler))
        );
        assert_eq!(Arc::strong_count(&handler), 3);
        drop(cleanup);
        assert_eq!(Arc::strong_count(&handler), 1);
        assert!(IRQ_STATE.lock().translated_hwirq(domain, hwirq).is_none());
    }
}

#[def_test(serial)]
fn nmi_controller_updates_hold_original_line_exclusion() -> unittest::TestResult {
    let virq = 0x5f49;
    let desc = IrqDesc::from_virq(virq);
    let line = Arc::new(IrqLine::new(IrqStateDesc::new(desc)));
    let has_atomic_exclusion = {
        let _guard = line.lock();
        line.is_state_locked_for_tests()
    };
    if !has_atomic_exclusion {
        return unittest::TestResult::Ignored;
    }
    let handler: Handler = Arc::new(|_| IrqEvent::HANDLED);
    assert!(IRQ_STATE.lock().descs.insert(virq, line.clone()).is_none());
    // This synthetic source is never enabled in hardware. Inject only the
    // bounded controller operations, retaining the real management path.
    // Do not probe global registry/table availability with try_lock here:
    // unrelated remote IRQ/NMI readers may own either lock even though this
    // transaction released it. Only the private Line's exclusion is observable
    // deterministically; registration/removal ownership is checked separately.
    let mut has_configured = false;
    let has_registered = super::register_nmi_with_mode(
        desc,
        handler.clone(),
        super::ManagementMode::Task,
        |stored| {
            has_configured = !karch::local_irq_enabled()
                && super::NMI_PUBLICATION_LOCK.is_locked()
                && line.is_state_locked_for_tests()
                && Arc::strong_count(&handler) == 2
                && stored.logical_irq() == Some(virq)
                && stored.flags.contains(crate::IrqFlags::PER_CPU);
            Ok(())
        },
    );
    let registered_action_count = line.lock().action_count();
    let registered_owner_count = Arc::strong_count(&handler);
    let mut has_masked = false;
    let has_unregistered = super::unregister_nmi_with(desc, |stored| {
        has_masked = !karch::local_irq_enabled()
            && super::NMI_PUBLICATION_LOCK.is_locked()
            && line.is_state_locked_for_tests()
            && stored.logical_irq() == Some(virq)
            && !stored.flags.contains(crate::IrqFlags::PER_CPU)
            && Arc::strong_count(&handler) == 3;
    });
    let remaining_actions = line.lock().action_count();
    let is_teardown_pending = line.lock().is_teardown_in_progress();
    let has_descriptor = crate::descriptor(virq).is_some();
    // Clean up synthetic ownership before reporting a failed observation.
    let table_owner = NMI_TABLE.lock().remove(&desc.hwirq);
    let remaining_line = IRQ_STATE.lock().descs.remove(&virq);
    let has_table_owner = table_owner.is_some();
    drop(table_owner);
    drop(remaining_line);
    drop(line);
    assert!(has_registered && has_unregistered);
    assert_eq!(registered_action_count, 1);
    assert_eq!(registered_owner_count, 3);
    assert!(has_configured);
    assert!(has_masked);
    assert_eq!(remaining_actions, 0);
    assert!(!is_teardown_pending);
    assert!(has_descriptor && !has_table_owner);
    assert_eq!(Arc::strong_count(&handler), 1);
    unittest::TestResult::Ok
}

#[def_test(serial)]
fn duplicate_nmi_on_another_line_does_not_publish_mapping() {
    let virq = 0x5f44;
    let other_virq = 0x5f45;
    let hwirq = crate::DYNAMIC_VIRQ_BASE + 0x5f44;
    let desc = IrqDesc::from_hwirq(hwirq).with_virq(virq);
    let other_desc = desc.with_virq(other_virq).with_domain(GIC_ROOT_DOMAIN);
    let prepared_other_desc = desc.with_virq(other_virq);
    let other_line = Arc::new(IrqLine::new(IrqStateDesc::new(prepared_other_desc)));
    let handler: Handler = Arc::new(|_| IrqEvent::HANDLED);
    let offered: Handler = Arc::new(|_| IrqEvent::HANDLED);
    let mut entry = IrqStateDesc::new(desc);
    assert!(entry.install_regular_action(IrqAction::nmi_fallback(handler.clone())));
    let line = Arc::new(IrqLine::new(entry));
    {
        let state = IRQ_STATE.lock();
        assert!(!state.descs.contains_key(&virq));
        assert!(!state.descs.contains_key(&other_virq));
        assert!(state.translated_hwirq(GIC_ROOT_DOMAIN, hwirq).is_none());
    }
    // Seed only software ownership. Both calls must reject before resolution
    // or platform configuration of this synthetic hardware number.
    assert!(IRQ_STATE.lock().descs.insert(virq, line.clone()).is_none());
    assert!(
        IRQ_STATE
            .lock()
            .descs
            .insert(other_virq, other_line.clone())
            .is_none()
    );
    assert!(NMI_TABLE.lock().insert(hwirq, handler.clone()).is_none());
    let task_rejected = !register_nmi(other_desc, offered.clone());
    let startup_rejected = !register_nmi_startup(other_desc, offered.clone());
    let has_no_mapping = {
        let state = IRQ_STATE.lock();
        state
            .descs
            .get(&other_virq)
            .is_some_and(|stored| Arc::ptr_eq(&stored, &other_line))
            && state.translated_hwirq(GIC_ROOT_DOMAIN, hwirq).is_none()
    };
    let other_unchanged = {
        let entry = other_line.lock();
        entry.desc == prepared_other_desc && entry.action_count() == 0
    };
    let original_desc = line.lock().desc;
    let action_count = line.lock().action_count();
    let table_owner = NMI_TABLE.lock().remove(&hwirq);
    let original_retained = table_owner
        .as_ref()
        .is_some_and(|owner| Arc::ptr_eq(owner, &handler));
    let removed_line = IRQ_STATE.lock().descs.remove(&virq);
    let removed_other = { IRQ_STATE.lock().remove_test_line(other_virq) };
    drop(removed_other);
    drop(removed_line);
    drop(line);
    drop(table_owner);
    assert!(task_rejected && startup_rejected);
    assert!(has_no_mapping && original_retained && other_unchanged);
    assert_eq!(original_desc, desc);
    assert_eq!(action_count, 1);
    assert_eq!(Arc::strong_count(&handler), 1);
    assert_eq!(Arc::strong_count(&offered), 1);
}

#[def_test(serial)]
fn regular_action_rejects_nmi_without_merging_richer_metadata() {
    let virq = 0x5f46;
    let hwirq = crate::DYNAMIC_VIRQ_BASE + 0x5f46;
    let desc = IrqDesc::from_hwirq(hwirq).with_virq(virq);
    let richer = desc
        .with_domain(GIC_ROOT_DOMAIN)
        .with_controller(IrqController::Gic)
        .with_source(IrqSource::Acpi);
    let handler: Handler = Arc::new(|_| IrqEvent::HANDLED);
    let offered: Handler = Arc::new(|_| IrqEvent::HANDLED);
    let mut entry = IrqStateDesc::new(desc);
    assert!(entry.install_regular_action(IrqAction::regular(handler.clone())));
    let line = Arc::new(IrqLine::new(entry));
    let generation_before = line.lock().generation_for_tests();
    {
        let state = IRQ_STATE.lock();
        assert!(!state.descs.contains_key(&virq));
        assert!(state.translated_hwirq(GIC_ROOT_DOMAIN, hwirq).is_none());
    }
    assert!(!NMI_TABLE.lock().contains_key(&hwirq));
    assert!(IRQ_STATE.lock().descs.insert(virq, line.clone()).is_none());
    let task_rejected = !register_nmi(richer, offered.clone());
    let startup_rejected = !register_nmi_startup(richer, offered.clone());
    let after = {
        let entry = line.lock();
        (
            entry.desc,
            entry.generation_for_tests(),
            entry.action_count(),
        )
    };
    let has_no_mapping = IRQ_STATE
        .lock()
        .translated_hwirq(GIC_ROOT_DOMAIN, hwirq)
        .is_none();
    let has_no_nmi = !NMI_TABLE.lock().contains_key(&hwirq);
    let removed_action = line.lock().take_regular_action();
    let removed_line = IRQ_STATE.lock().descs.remove(&virq);
    drop(removed_action);
    drop(removed_line);
    drop(line);
    assert!(task_rejected && startup_rejected);
    assert_eq!(after, (desc, generation_before, 1));
    assert!(has_no_mapping && has_no_nmi);
    assert_eq!(Arc::strong_count(&handler), 1);
    assert_eq!(Arc::strong_count(&offered), 1);
}

#[def_test(serial)]
fn nmi_context_rejects_management_before_publication() {
    let virq = 0x5f40;
    let desc = IrqDesc::from_virq(virq);
    let handler: Handler = Arc::new(|_| IrqEvent::HANDLED);
    assert!(crate::descriptor(virq).is_none());
    let observed = {
        let _pin = kspin::NoPreemptIrqSave::new();
        // SAFETY: this synthetic NMI excludes IRQs/migration and only calls
        // management rejection paths. It neither dispatches hardware nor waits.
        unsafe {
            with_nmi_context(|| {
                [
                    is_invalid_context(crate::try_map(desc)),
                    is_invalid_context(crate::try_enable(virq, true)),
                    is_invalid_context(crate::try_enable_irq(virq)),
                    is_invalid_context(crate::try_disable_irq_nosync(virq)),
                    is_invalid_context(crate::try_disable_irq(virq)),
                    is_invalid_context(crate::try_synchronize_irq(virq)),
                    is_invalid_context(crate::try_register(virq, handler.clone())),
                    is_invalid_context(crate::try_register_disabled(virq, handler.clone())),
                    is_invalid_context(crate::try_register_shared(virq, handler.clone())),
                    is_invalid_context(crate::try_free_irq(virq)),
                    is_invalid_context(crate::try_free_irq_action(virq, IrqActionToken::new(0))),
                    !register_nmi(desc, handler.clone()),
                    !unregister_nmi(desc),
                    !crate::register(virq, handler.clone()),
                    crate::free_irq(virq).is_none(),
                    crate::alloc_msix(crate::IrqAffinity::Any).is_none(),
                    !crate::free_msix(virq),
                ]
            })
        }
    };
    assert!(observed.into_iter().all(|is_rejected| is_rejected));
    assert!(crate::descriptor(virq).is_none());
    assert!(!NMI_TABLE.lock().contains_key(&desc.hwirq));
    assert_eq!(Arc::strong_count(&handler), 1);
}

#[def_test(serial)]
fn ordinary_free_cannot_withdraw_nmi_fallback() {
    let virq = 0x5f41;
    let handler: Handler = Arc::new(|_| IrqEvent::HANDLED);
    let mut entry = IrqStateDesc::new(IrqDesc::from_virq(virq));
    assert!(entry.install_regular_action(IrqAction::nmi_fallback(handler.clone())));
    let line = Arc::new(IrqLine::new(entry));
    assert!(IRQ_STATE.lock().descs.insert(virq, line.clone()).is_none());
    assert!(NMI_TABLE.lock().insert(virq, handler.clone()).is_none());

    let was_nmi_removal_rejected = {
        let _pin = kspin::NoPreemptIrqSave::new();
        // SAFETY: balanced synthetic NMI under IRQ/preemption exclusion. Only
        // the rejection path is exercised; this number has no hardware source.
        unsafe { with_nmi_context(|| !unregister_nmi(IrqDesc::from_virq(virq))) }
    };

    let regular_removed = crate::try_free_irq(virq).unwrap();
    let token_removed = crate::try_free_irq_action(virq, IrqActionToken::new(0)).unwrap();
    let (count, depth, removed) = {
        let mut entry = line.lock();
        let count = entry.action_count();
        let depth = entry.teardown_depth_for_tests();
        (count, depth, entry.take_nmi_fallback())
    };
    IRQ_STATE.lock().descs.remove(&virq);
    let table_owner = NMI_TABLE.lock().remove(&virq);
    assert!(was_nmi_removal_rejected);
    assert!(table_owner.is_some());
    assert!(regular_removed.is_none());
    assert!(token_removed.is_none());
    assert_eq!(count, 1);
    assert_eq!(depth, 0);
    assert!(removed.is_some());
    drop(removed);
    drop(table_owner);
    assert_eq!(Arc::strong_count(&handler), 1);
}

#[def_test]
fn nmi_removal_cannot_take_an_ordinary_regular_action() {
    let handler: Handler = Arc::new(|_| IrqEvent::HANDLED);
    let mut entry = IrqStateDesc::new(IrqDesc::from_virq(0x5f42));
    assert!(entry.install_regular_action(IrqAction::regular(handler.clone())));
    assert!(entry.take_nmi_fallback().is_none());
    assert_eq!(entry.action_count(), 1);
    let removed = entry.take_regular_action();
    assert!(removed.is_some());
    drop(removed);
    assert_eq!(Arc::strong_count(&handler), 1);
}

struct DropProbe(Arc<AtomicBool>);

impl crate::IrqHandler for DropProbe {
    fn handle(&self, _irq: usize) -> IrqEvent {
        IrqEvent::HANDLED
    }
}

impl Drop for DropProbe {
    fn drop(&mut self) {
        // Withdrawal nests the raw table guard inside publication exclusion.
        // Check local IRQ restoration before reentry, then tolerate remote
        // contention. Probe the enclosing publication lock, not the raw table:
        // NMI dispatch never takes publication exclusion.
        let has_unlocked_access = if karch::local_irq_enabled() {
            drop(IRQ_STATE.lock());
            drop(super::NMI_PUBLICATION_LOCK.lock());
            true
        } else {
            false
        };
        self.0.store(has_unlocked_access, Ordering::Release);
    }
}

#[def_test]
fn publication_lock_preserves_the_callers_irq_mask() {
    assert!(karch::local_irq_enabled());
    {
        let _publication = super::NMI_PUBLICATION_LOCK.lock();
        assert!(karch::local_irq_enabled());
    }
    {
        let _irq = kspin::IrqSave::new();
        {
            let _publication = super::NMI_PUBLICATION_LOCK.lock();
            assert!(!karch::local_irq_enabled());
        }
        assert!(!karch::local_irq_enabled());
    }
    assert!(karch::local_irq_enabled());
}

#[def_test]
fn nmi_drop_probe_rejects_masked_context_without_reentry() {
    let has_unlocked_drop = Arc::new(AtomicBool::new(true));
    let probe = DropProbe(has_unlocked_drop.clone());
    {
        let _irq = kspin::IrqSave::new();
        drop(probe);
    }
    assert!(!has_unlocked_drop.load(Ordering::Acquire));
}

#[def_test(serial)]
fn nmi_removal_drops_client_after_publication_and_state_unlock() {
    assert!(karch::local_irq_enabled());
    let virq = 0x5f43;
    let desc = IrqDesc::from_virq(virq);
    let has_unlocked_drop = Arc::new(AtomicBool::new(false));
    let handler: Handler = Arc::new(DropProbe(has_unlocked_drop.clone()));
    let mut entry = IrqStateDesc::new(desc);
    assert!(entry.install_regular_action(IrqAction::nmi_fallback(handler.clone())));
    let line = Arc::new(IrqLine::new(entry));
    assert!(IRQ_STATE.lock().descs.insert(virq, line).is_none());
    // No device routes this synthetic number. Seed the two owners without
    // configuring or enabling a platform NMI source.
    assert!(NMI_TABLE.lock().insert(desc.hwirq, handler).is_none());
    let removed = unregister_nmi(desc);
    assert!(removed);
    assert!(crate::descriptor(virq).is_some());
    assert!(!NMI_TABLE.lock().contains_key(&desc.hwirq));
    assert!(has_unlocked_drop.load(Ordering::Acquire));
    let retired = IRQ_STATE.lock().remove_test_line(virq);
    drop(retired);
}

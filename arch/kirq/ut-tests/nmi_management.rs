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

#[def_test(serial)]
fn duplicate_nmi_on_another_line_does_not_publish_mapping() {
    let virq = 0x5f44;
    let other_virq = 0x5f45;
    let hwirq = crate::DYNAMIC_VIRQ_BASE + 0x5f44;
    let desc = IrqDesc::from_hwirq(hwirq).with_virq(virq);
    let other_desc = desc.with_virq(other_virq).with_domain(GIC_ROOT_DOMAIN);
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
    assert!(NMI_TABLE.lock().insert(hwirq, handler.clone()).is_none());
    let task_rejected = !register_nmi(other_desc, offered.clone());
    let startup_rejected = !register_nmi_startup(other_desc, offered.clone());
    let has_no_mapping = {
        let state = IRQ_STATE.lock();
        !state.descs.contains_key(&other_virq)
            && state.translated_hwirq(GIC_ROOT_DOMAIN, hwirq).is_none()
    };
    let original_desc = line.lock().desc;
    let action_count = line.lock().action_count();
    let table_owner = NMI_TABLE.lock().remove(&hwirq);
    let original_retained = table_owner
        .as_ref()
        .is_some_and(|owner| Arc::ptr_eq(owner, &handler));
    let removed_line = IRQ_STATE.lock().descs.remove(&virq);
    drop(removed_line);
    drop(line);
    drop(table_owner);
    assert!(task_rejected && startup_rejected);
    assert!(has_no_mapping && original_retained);
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
        (entry.desc, entry.generation_for_tests(), entry.action_count())
    };
    let has_no_mapping = IRQ_STATE.lock().translated_hwirq(GIC_ROOT_DOMAIN, hwirq).is_none();
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
        let has_state_access = IRQ_STATE.try_lock().is_some();
        let has_table_access = NMI_TABLE.try_lock().is_some();
        self.0.store(has_state_access && has_table_access, Ordering::Release);
    }
}

#[def_test(serial)]
fn nmi_removal_drops_client_after_table_and_state_unlock() {
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
    assert!(crate::descriptor(virq).is_none());
    assert!(!NMI_TABLE.lock().contains_key(&desc.hwirq));
    assert!(has_unlocked_drop.load(Ordering::Acquire));
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use unittest::{assert, assert_eq, def_test};

use super::{IRQ_STATE, IrqLine, IrqState, IrqStateDesc};
use crate::{IrqDesc, IrqEvent, IrqRef, action::IrqAction, domain::TEST_DOMAIN};

struct TestMapping(usize);

#[def_test]
fn missing_prepared_capacity_cannot_publish_half_a_mapping() {
    let virq = 0x5fb5;
    let desc = IrqDesc::from_hwirq(0x5fb6)
        .with_domain(TEST_DOMAIN)
        .with_virq(virq);
    let line = Arc::new(IrqLine::new(IrqStateDesc::new(desc)));
    let mut state = IrqState::new();
    let mut prepared = super::PreparedPublication::new(virq, None, true).unwrap();
    let resolution = state.plan_resolution(desc.into()).unwrap();
    let result = state.commit_resolution(resolution, &line, &mut line.lock(), &mut prepared);
    assert!(matches!(result, Err(crate::IrqDescError::NoMemory)));
    assert!(state.descs.get(&virq).is_none());
    assert!(state.domain_virq(TEST_DOMAIN, desc.hwirq).is_none());
    assert!(line.logical_irq().is_none());
    assert!(prepared.logical.is_some());
}

#[def_test]
fn domain_dispatch_borrows_the_domain_owner_directly() {
    let hwirq = 0x5fb2;
    let desc = IrqDesc::from_hwirq(hwirq).with_domain(TEST_DOMAIN);
    let line = Arc::new(IrqLine::new(IrqStateDesc::new(desc)));
    let mut state = IrqState::new();
    let mapped = state
        .try_resolve_desc(desc, Some(&line))
        .unwrap()
        .into_desc();
    let virq = mapped.logical_irq().unwrap();
    // No global snapshot is published by this private registry. A normal
    // claim must borrow the Domain's owner, not translate then query descs.
    let (resolved, retained) = state
        .lookup_dispatch_line(IrqRef::Domain(TEST_DOMAIN, hwirq))
        .unwrap();
    let retained = retained.unwrap();
    assert_eq!(resolved, virq);
    assert!(Arc::ptr_eq(&retained, &line));
    let same_allocation = krcu::with_read(|scope| {
        let mapped = state.domain_states[crate::domain::slot(TEST_DOMAIN).unwrap()]
            .array()
            .read(scope, hwirq)
            .unwrap();
        core::ptr::eq(mapped, Arc::as_ptr(&retained))
            && core::ptr::eq(state.descs.array().read(scope, virq).unwrap(), mapped)
    });
    assert!(same_allocation);
    // An owned lookup result keeps the same identity after a later lookup.
    let retained = retained.clone();
    assert_eq!(
        state.try_resolve_desc(mapped, None).unwrap().into_desc(),
        mapped
    );
    assert_eq!(retained.logical_irq(), Some(virq));
    assert!(Arc::ptr_eq(&retained, &line));
}

#[def_test]
fn dispatch_lookup_preserves_strict_and_identity_misses() {
    let virq = 0x5fb3;
    let desc = IrqDesc::from_virq(virq);
    let line = Arc::new(IrqLine::new(IrqStateDesc::new(desc)));
    let mut state = IrqState::new();
    state.try_resolve_desc(desc, Some(&line)).unwrap();
    assert!(
        state
            .lookup_dispatch_line(IrqRef::Domain(TEST_DOMAIN, virq))
            .is_none()
    );
    assert!(
        state
            .lookup_dispatch_line(IrqRef::Domain(crate::IrqDomainId::new(u32::MAX - 1), virq))
            .is_none()
    );
    for source in [
        IrqRef::Virq(virq),
        IrqRef::Domain(crate::GIC_ROOT_DOMAIN, virq),
    ] {
        let (resolved, retained) = state.lookup_dispatch_line(source).unwrap();
        assert_eq!(resolved, virq);
        assert!(Arc::ptr_eq(&retained.unwrap(), &line));
    }
    for source in [
        IrqRef::Virq(virq + 1),
        IrqRef::Domain(crate::GIC_ROOT_DOMAIN, virq + 1),
    ] {
        let (resolved, retained) = state.lookup_dispatch_line(source).unwrap();
        assert_eq!(resolved, virq + 1);
        assert!(retained.is_none());
    }
}

impl Drop for TestMapping {
    fn drop(&mut self) {
        // This test Domain has no device producer. Remove its fixture and
        // republish its reverse map, then drop any retained owners unlocked.
        let removed = { IRQ_STATE.lock().remove_test_line(self.0) };
        drop(removed);
    }
}

#[def_test]
fn published_line_cannot_alias_another_logical_number() {
    let original = IrqDesc::from_virq(0x5fb0);
    let alias = IrqDesc::from_virq(0x5fb1);
    let line = Arc::new(IrqLine::new(IrqStateDesc::new(original)));
    let mut state = IrqState::new();
    assert_eq!(line.logical_irq(), None);
    state.try_resolve_desc(original, Some(&line)).unwrap();
    {
        let entry = line.lock();
        // Identity observation must not acquire the mutable Line lock again.
        assert_eq!(line.logical_irq(), Some(original.hwirq));
        assert_eq!(entry.desc, original);
    }
    assert!(matches!(
        state.try_resolve_desc(alias, Some(&line)),
        Err(crate::IrqDescError::UnknownIrq)
    ));
    assert_eq!(state.descs.len(), 1);
    assert!(Arc::ptr_eq(
        &state.descs.get(&original.hwirq).unwrap(),
        &line
    ));
    assert!(!state.descs.contains_key(&alias.hwirq));
    assert_eq!(line.logical_irq(), Some(original.hwirq));
    assert_eq!(line.lock().desc, original);
}

#[def_test(serial)]
fn action_removal_keeps_domain_line_and_both_indexes() {
    for is_shared in [false, true] {
        let hwirq = 0x5fa4 + usize::from(is_shared);
        let desc = IrqDesc::from_hwirq(hwirq).with_domain(TEST_DOMAIN);
        let virq = crate::try_map(desc).unwrap();
        let cleanup = TestMapping(virq);
        let line = IRQ_STATE.lock().descs.get(&virq).unwrap();
        let handler: crate::Handler = Arc::new(|_| IrqEvent::HANDLED);
        // Install only software ownership on a never-enabled synthetic Line;
        // real free_irq/free_irq_action must therefore perform no device I/O.
        let token = {
            let mut entry = line.lock();
            if is_shared {
                Some(entry.install_shared_action(handler.clone()).unwrap())
            } else {
                assert!(entry.install_regular_action(IrqAction::regular(handler.clone())));
                None
            }
        };
        let removed = match token {
            Some(token) => crate::try_free_irq_action(virq, token),
            None => crate::try_free_irq(virq),
        }
        .unwrap();
        assert!(removed.is_some());
        drop(removed);
        assert_eq!(Arc::strong_count(&handler), 1);
        assert!(line.lock().is_unused());
        assert!(!line.lock().is_teardown_in_progress());
        {
            let state = IRQ_STATE.lock();
            assert!(Arc::ptr_eq(&state.descs.get(&virq).unwrap(), &line));
            assert_eq!(state.domain_virq(TEST_DOMAIN, hwirq), Some(virq));
            assert_eq!(state.mapping_for_virq(virq), Some((TEST_DOMAIN, hwirq)));
        }
        assert_eq!(crate::translate_hwirq(TEST_DOMAIN, hwirq), Some(virq));
        assert_eq!(crate::try_map(desc), Ok(virq));
        let remapped = IRQ_STATE.lock().descs.get(&virq).unwrap();
        assert!(Arc::ptr_eq(&remapped, &line));
        drop(cleanup);
    }
}

#[def_test(serial)]
fn empty_claim_completion_keeps_domain_line() {
    let hwirq = 0x5fa6;
    let virq = crate::try_map(IrqDesc::from_hwirq(hwirq).with_domain(TEST_DOMAIN)).unwrap();
    let cleanup = TestMapping(virq);
    let line = IRQ_STATE.lock().descs.get(&virq).unwrap();
    let mut completion_count = 0;
    let mut in_flight_at_completion = 0;
    let resolved = crate::dispatch::dispatch_claim(IrqRef::Domain(TEST_DOMAIN, hwirq), || {
        completion_count += 1;
        in_flight_at_completion = line.lock().in_flight();
    });
    assert_eq!(resolved, Some(virq));
    assert_eq!(completion_count, 1);
    assert_eq!(in_flight_at_completion, 1);
    assert_eq!(line.lock().in_flight(), 0);
    let retained = IRQ_STATE.lock().descs.get(&virq).unwrap();
    assert!(Arc::ptr_eq(&retained, &line));
    assert_eq!(crate::translate_hwirq(TEST_DOMAIN, hwirq), Some(virq));
    drop(cleanup);
}

#[def_test]
fn domain_enrichment_and_management_completion_preserve_identity() {
    let virq = 0x5fa7;
    let plain = IrqDesc::from_virq(virq);
    let mapped = plain.with_domain(TEST_DOMAIN);
    let line = Arc::new(IrqLine::new(IrqStateDesc::new(plain)));
    let mut state = IrqState::new();
    state.try_resolve_desc(plain, Some(&line)).unwrap();
    line.lock().begin_management();
    line.lock().begin_teardown();
    // Completion must not undo enrichment. Private registry only: no global
    // snapshot or controller I/O.
    state.try_resolve_desc(mapped, None).unwrap();
    assert_eq!(line.logical_irq(), Some(virq));
    line.lock().finish_management(true);
    assert!(line.lock().is_unused());
    assert!(state.remove_if_unused(virq).is_none());
    assert!(Arc::ptr_eq(&state.descs.get(&virq).unwrap(), &line));
    assert_eq!(state.domain_virq(TEST_DOMAIN, virq), Some(virq));
    assert_eq!(state.mapping_for_virq(virq), Some((TEST_DOMAIN, virq)));
}

#[def_test(serial)]
fn bare_preparation_survives_action_removal_and_startup_reregistration() {
    for is_shared in [false, true] {
        let virq = crate::DYNAMIC_VIRQ_BASE + 0x5fae + usize::from(is_shared);
        assert_eq!(crate::try_map(IrqDesc::from_virq(virq)), Ok(virq));
        let _cleanup = TestMapping(virq);
        let line = IRQ_STATE.lock().descs.get(&virq).unwrap();
        let handler: crate::Handler = Arc::new(|_| IrqEvent::HANDLED);
        // Synthetic, never-enabled source: install ownership without touching
        // hardware, then exercise the public synchronous removal entrances.
        let token = {
            let mut entry = line.lock();
            if is_shared {
                Some(entry.install_shared_action(handler.clone()).unwrap())
            } else {
                assert!(entry.install_regular_action(IrqAction::regular(handler.clone())));
                None
            }
        };
        let removed = match token {
            Some(token) => crate::try_free_irq_action(virq, token),
            None => crate::try_free_irq(virq),
        }
        .unwrap();
        assert!(removed.is_some());
        drop(removed);
        assert_eq!(Arc::strong_count(&handler), 1);
        assert!(line.lock().is_unused());
        assert!(
            IRQ_STATE
                .lock()
                .descs
                .get(&virq)
                .is_some_and(|stored| Arc::ptr_eq(&stored, &line))
        );
        assert_eq!(
            crate::try_register_startup(virq, handler, crate::IrqRegisterOptions::disabled(),),
            Ok(true)
        );
        assert_eq!(line.lock().action_count(), 1);
        assert!(crate::try_free_irq(virq).unwrap().is_some());
    }
}

#[def_test(serial)]
fn empty_claim_keeps_prepared_bare_line_for_startup_registration() {
    let virq = crate::DYNAMIC_VIRQ_BASE + 0x5fad;
    assert!(crate::descriptor(virq).is_none());
    assert_eq!(crate::try_map(IrqDesc::from_virq(virq)), Ok(virq));
    let _cleanup = TestMapping(virq);
    let line = IRQ_STATE.lock().descs.get(&virq).unwrap();
    let completion = line.lock().in_flight_zero_completion();
    let mut in_flight_at_completion = 0;
    let mut completions = 0;
    assert_eq!(
        crate::dispatch::dispatch_claim(IrqRef::Virq(virq), || {
            completions += 1;
            in_flight_at_completion = line.lock().in_flight();
        }),
        Some(virq)
    );
    assert_eq!(completions, 1);
    assert_eq!(in_flight_at_completion, 1);
    assert_eq!(line.lock().in_flight(), 0);
    assert!(completion.is_completed());
    assert!(
        IRQ_STATE
            .lock()
            .descs
            .get(&virq)
            .is_some_and(|retained| Arc::ptr_eq(&retained, &line))
    );
    // No hardware metadata or producer: exercise the actual startup admission
    // after an empty claim, without configuring a real controller source.
    assert_eq!(
        crate::try_register_startup(
            virq,
            Arc::new(|_| IrqEvent::HANDLED),
            crate::IrqRegisterOptions::disabled(),
        ),
        Ok(true)
    );
    assert_eq!(line.lock().action_count(), 1);
    assert!(crate::try_free_irq(virq).unwrap().is_some());
}

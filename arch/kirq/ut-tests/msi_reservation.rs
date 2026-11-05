// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use unittest::{assert, assert_eq, def_test};

use super::{IrqLine, IrqState, IrqStateDesc, MsiReservation};
use crate::{IrqDesc, IrqDescError, IrqFlags};

fn candidate(desc: IrqDesc) -> Arc<IrqLine> {
    let mut entry = IrqStateDesc::new(desc);
    entry.msi_reservation = Some(MsiReservation::for_test(desc.hwirq));
    Arc::new(IrqLine::new(entry))
}

#[def_test]
fn withdrawn_line_cannot_be_republished_or_accept_actions() {
    // Private registry and synthetic reservation: no Domain snapshot or MMIO.
    let desc = IrqDesc::from_virq(0x5fa4).with_flags(IrqFlags::MSI);
    let line = candidate(desc);
    let mut state = IrqState::new();
    state.try_resolve_desc(desc, Some(&line)).unwrap();
    let mut released = None;
    let removed = super::release_unused_msi(&mut state, desc.hwirq, |_, owner| {
        released = Some((owner.hwirq(), line.lock().is_retired()));
    })
    .unwrap();
    assert_eq!(released, Some((desc.hwirq, true)));
    assert_eq!(line.logical_irq(), Some(desc.hwirq));
    assert!(Arc::ptr_eq(&line, &removed));
    assert!(matches!(
        state.try_resolve_desc(desc, Some(&line)),
        Err(IrqDescError::UnknownIrq)
    ));
    assert!(state.descs.is_empty());
    {
        let mut entry = line.lock();
        assert!(!entry.can_install_regular_action());
        assert!(!entry.can_install_shared_action());
        assert!(
            !entry.install_regular_action(crate::action::IrqAction::regular(Arc::new(|_| {
                crate::IrqEvent::HANDLED
            }),))
        );
        assert!(
            entry
                .install_shared_action(Arc::new(|_| crate::IrqEvent::HANDLED))
                .is_none()
        );
        assert_eq!(entry.action_count(), 0);
        assert_eq!(entry.in_flight(), 0);
    }
    // Reusing a logical number requires a fresh identity, not the old owner.
    let successor = candidate(desc);
    state.try_resolve_desc(desc, Some(&successor)).unwrap();
    assert!(Arc::ptr_eq(
        state.descs.get(&desc.hwirq).unwrap(),
        &successor
    ));
    assert!(!Arc::ptr_eq(&line, &successor));
    assert!(!successor.lock().is_retired());
    assert_eq!(successor.logical_irq(), line.logical_irq());
    assert!(successor.lock().can_install_regular_action());
}

#[def_test]
fn publication_failure_retains_private_reservation() {
    let desc = IrqDesc::from_hwirq(0x5fa0)
        .with_domain(crate::domain::TEST_DOMAIN)
        .with_flags(IrqFlags::MSI);
    let line = candidate(desc);
    let mut state = IrqState::new();
    state.set_next_virq_for_tests(usize::MAX);
    assert!(matches!(
        state.insert_msi_reservation(desc, &line),
        Err(IrqDescError::VirqExhausted { .. })
    ));
    assert_eq!(line.logical_irq(), None);
    assert!(state.descs.is_empty());
    assert!(
        state
            .domain_virq(crate::domain::TEST_DOMAIN, desc.hwirq)
            .is_none()
    );
    assert_eq!(
        line.lock().msi_reservation.as_ref().unwrap().hwirq(),
        desc.hwirq
    );
    assert_eq!(Arc::strong_count(&line), 1);
}

#[def_test]
fn new_allocation_never_adopts_an_existing_mapping() {
    let desc = IrqDesc::from_hwirq(0x5fa1)
        .with_domain(crate::domain::TEST_DOMAIN)
        .with_flags(IrqFlags::MSI);
    let existing = candidate(desc);
    let offered = candidate(desc);
    let mut state = IrqState::new();
    let virq = state
        .insert_msi_reservation(desc, &existing)
        .unwrap()
        .into_desc()
        .logical_irq()
        .unwrap();
    assert!(matches!(
        state.insert_msi_reservation(desc, &offered),
        Err(IrqDescError::MappingConflict { .. })
    ));
    assert_eq!(offered.logical_irq(), None);
    assert_eq!(existing.logical_irq(), Some(virq));
    assert!(Arc::ptr_eq(state.descs.get(&virq).unwrap(), &existing));
    assert_eq!(
        state.domain_virq(crate::domain::TEST_DOMAIN, desc.hwirq),
        Some(virq)
    );
    assert_eq!(
        offered.lock().msi_reservation.as_ref().unwrap().hwirq(),
        desc.hwirq
    );
    assert_eq!(Arc::strong_count(&offered), 1);
    assert_eq!(
        existing.lock().msi_reservation.as_ref().unwrap().hwirq(),
        desc.hwirq
    );
}

struct RestoreSnapshot(Option<crate::domain::PreparedPublication>);

impl RestoreSnapshot {
    fn new() -> Self {
        let state = super::IRQ_STATE.lock();
        let entries: alloc::vec::Vec<_> = state
            .domain_mapping_entries(crate::domain::TEST_DOMAIN)
            .collect();
        Self(Some(
            crate::domain::prepare_snapshot(crate::domain::TEST_DOMAIN, &entries).unwrap(),
        ))
    }
}

impl Drop for RestoreSnapshot {
    fn drop(&mut self) {
        // Prepare restoration before the fixture changes publication, so even
        // a failed assertion restores it without allocating on early return.
        let _state = super::IRQ_STATE.lock();
        if let Some(publication) = self.0.take() {
            publication.publish();
        }
    }
}

#[def_test(serial)]
fn enabled_msi_without_actions_cannot_release_its_reservation() {
    let _restore = RestoreSnapshot::new();
    let domain = crate::domain::TEST_DOMAIN;
    let desc = IrqDesc::from_hwirq(0x5fa3)
        .with_domain(domain)
        .with_flags(IrqFlags::MSI);
    let line = candidate(desc);
    let mut state = IrqState::new();
    let resolved = state.insert_msi_reservation(desc, &line).unwrap();
    let virq = super::publish_resolution(resolved).logical_irq().unwrap();
    {
        let mut entry = line.lock();
        // Model the legacy force-enable entrance without programming hardware.
        // No device is ever given this synthetic reservation.
        let plan = entry.prepare_legacy_enable();
        entry.commit_platform_plan(plan);
        assert_eq!(entry.action_count(), 0);
        assert!(entry.is_enabled_for_tests());
    }
    let mut releases = 0;
    let rejected = super::release_unused_msi(&mut state, virq, |_, _| releases += 1);
    assert!(rejected.is_none());
    assert_eq!(releases, 0);
    assert!(!line.lock().is_retired());
    assert!(Arc::ptr_eq(state.descs.get(&virq).unwrap(), &line));
    assert_eq!(state.mapping_for_virq(virq), Some((domain, desc.hwirq)));
    assert_eq!(state.domain_virq(domain, desc.hwirq), Some(virq));
    assert_eq!(crate::translate_hwirq(domain, desc.hwirq), Some(virq));
    {
        let mut entry = line.lock();
        assert!(entry.is_enabled_for_tests());
        assert_eq!(entry.disable_depth_for_tests(), 0);
        assert_eq!(entry.msi_reservation.as_ref().unwrap().hwirq(), desc.hwirq);
        let plan = entry.prepare_disable_irq_nosync();
        entry.commit_platform_plan(plan);
    }
    let mut observed = None;
    let removed = super::release_unused_msi(&mut state, virq, |registry, reservation| {
        observed = Some((
            registry.descs.contains_key(&virq),
            registry.domain_virq(domain, desc.hwirq),
            crate::translate_hwirq(domain, desc.hwirq),
            reservation.hwirq(),
        ));
        releases += 1;
    });
    assert_eq!(observed, Some((false, None, None, desc.hwirq)));
    assert!(
        removed
            .as_ref()
            .is_some_and(|owner| Arc::ptr_eq(owner, &line))
    );
    assert_eq!(releases, 1);
    assert!(line.lock().msi_reservation.is_none());
}

#[def_test(serial)]
fn release_withdraws_both_indexes_before_returning_backend_ownership() {
    // This serial fixture temporarily publishes only the test domain. Restore
    // its real registry snapshot on every exit; no hardware source uses it.
    let _restore = RestoreSnapshot::new();
    let domain = crate::domain::TEST_DOMAIN;
    let desc = IrqDesc::from_hwirq(0x5fa2)
        .with_domain(domain)
        .with_flags(IrqFlags::MSI);
    let line = candidate(desc);
    let mut state = IrqState::new();
    let resolved = state.insert_msi_reservation(desc, &line).unwrap();
    let virq = super::publish_resolution(resolved).logical_irq().unwrap();
    assert_eq!(crate::translate_hwirq(domain, desc.hwirq), Some(virq));
    let mut releases = 0;
    let mut observed = None;
    let removed = super::release_unused_msi(&mut state, virq, |registry, reservation| {
        observed = Some((
            registry.descs.contains_key(&virq),
            registry.mapping_for_virq(virq),
            registry.domain_virq(domain, desc.hwirq),
            crate::translate_hwirq(domain, desc.hwirq),
            reservation.hwirq(),
        ));
        releases += 1;
    })
    .unwrap();
    assert_eq!(observed, Some((false, None, None, None, desc.hwirq)));
    assert!(Arc::ptr_eq(&removed, &line));
    assert_eq!(releases, 1);
    assert!(super::release_unused_msi(&mut state, virq, |_, _| releases += 1).is_none());
    assert_eq!(releases, 1);
    assert!(line.lock().msi_reservation.is_none());
}

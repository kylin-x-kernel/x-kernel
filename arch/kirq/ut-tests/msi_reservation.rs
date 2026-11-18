// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use unittest::{assert, assert_eq, def_test};

use super::{IrqLine, IrqState, IrqStateDesc, MsiReservation};
use crate::{IrqDesc, IrqDescError, IrqFlags};

/// A synthetic MSI identity with real global indexes and RCU retirement.
pub struct RcuIndexProbe {
    virq: usize,
    hwirq: usize,
    owner: alloc::sync::Weak<IrqLine>,
}

impl RcuIndexProbe {
    /// Publish a test-owned number without enabling any hardware source.
    pub fn new(virq: usize, hwirq: usize) -> Self {
        let desc = IrqDesc::new(hwirq, crate::IrqTrigger::EdgeRising)
            .with_domain(crate::domain::TEST_DOMAIN)
            .with_virq(virq)
            .with_flags(IrqFlags::MSI);
        let candidate = Arc::new(IrqLine::new(IrqStateDesc::test_new(desc)));
        super::IRQ_STATE
            .lock()
            .try_resolve_desc(desc, Some(&candidate))
            .unwrap();
        let line = super::IRQ_STATE.lock().descs.get(&virq).unwrap();
        let withdrawal = super::PreparedWithdrawal::new(hwirq).unwrap();
        line.lock().msi_withdrawal = Some(withdrawal);
        line.lock().msi_reservation = Some(MsiReservation::for_test(hwirq));
        Self {
            virq,
            hwirq,
            owner: Arc::downgrade(&line),
        }
    }

    /// Keep borrowed references, not owned Arcs, across the supplied handshake.
    pub fn read_across(&self, body: impl FnOnce()) -> bool {
        krcu::with_read(|scope| {
            let directory = crate::runtime::index::DIRECTORY.read(scope, self.virq);
            let domain = crate::domain::domain(crate::domain::TEST_DOMAIN).unwrap();
            let mapping = domain.lines.read(scope, self.hwirq);
            body();
            directory.zip(mapping).is_some_and(|(left, right)| {
                core::ptr::eq(left, right)
                    && core::ptr::eq(left, self.owner.as_ptr())
                    && left.logical_irq() == Some(self.virq)
            })
        })
    }

    /// Retain actual interrupt borrows across a remote withdrawal/reuse handshake.
    /// The body must neither block nor schedule, and the remote writer must not
    /// wait for this CPU's IRQ delivery before releasing the handshake.
    pub fn read_interrupt_across(&self, body: impl FnOnce()) -> bool {
        let _pin = kspin::NoPreemptIrqSave::new();
        let watching = krcu_core::RcuContextIf::current_cpu_watching();
        let depth = watching.interrupt_depth_for_test();
        let was_reading = krcu::is_reading();
        let valid = super::tests_index_transactions::with_irq(|| {
            krcu::with_interrupt_read(|scope| {
                let directory = crate::runtime::index::DIRECTORY.read(scope, self.virq);
                let mapping = crate::domain::TEST_IRQ_DOMAIN.lines.read(scope, self.hwirq);
                let source = crate::IrqRef::Domain(crate::domain::TEST_DOMAIN, self.hwirq);
                let retained_matches = crate::runtime::index::lookup(source)
                    .and_then(|(_, line)| line)
                    .is_some_and(|line| core::ptr::eq(Arc::as_ptr(&line), self.owner.as_ptr()));
                // SAFETY: this strictly nested synthetic NMI runs under the
                // outer IRQ fixture's CPU exclusion, without scheduling.
                let numeric_matches = unsafe {
                    crate::runtime::rcu::with_nmi_context(|| {
                        crate::translate_hwirq(crate::domain::TEST_DOMAIN, self.hwirq)
                            == Some(self.virq)
                    })
                };
                body();
                let successor_matches = crate::runtime::index::lookup(source)
                    .and_then(|(_, line)| line)
                    .is_some_and(|line| !core::ptr::eq(Arc::as_ptr(&line), self.owner.as_ptr()));
                directory.zip(mapping).is_some_and(|(left, right)| {
                    core::ptr::eq(left, right)
                        && core::ptr::eq(left, self.owner.as_ptr())
                        && left.logical_irq() == Some(self.virq)
                }) && retained_matches
                    && numeric_matches
                    && successor_matches
            })
        });
        valid
            && watching.interrupt_depth_for_test() == depth
            && krcu::is_reading() == was_reading
            && !crate::context::is_in_nmi()
            && !crate::context::is_in_hardirq()
    }

    /// Returns a fresh handle for the exact current Line, if any.
    pub fn handle(&self) -> Option<crate::IrqLineHandle> {
        crate::lookup_irq_line(self.virq).ok().flatten()
    }

    pub fn mark_action_history_for_tests(&self) {
        self.owner
            .upgrade()
            .expect("probe owner")
            .lock()
            .set_action_history_for_tests();
    }

    /// Runs the x86 production free path for this synthetic reservation.
    pub fn free_msi_via_production(&self) -> bool {
        super::free_msi_if_unused(self.virq, |_reservation| {})
    }

    /// Withdraw only this identity and submit both owners after unlocking.
    ///
    /// No real source is associated with the fixture.
    pub fn retire(&self) -> bool {
        if krcu::preflight_callback_submission().is_err() {
            return false;
        }
        let withdrawn = {
            let mut state = super::IRQ_STATE.lock();
            if !state
                .descs
                .get(&self.virq)
                .is_some_and(|line| core::ptr::eq(Arc::as_ptr(&line), self.owner.as_ptr()))
            {
                return false;
            }
            // SAFETY: successful preflight precedes this guard; no callbacks or
            // read scopes intervene before unlocked retirement below.
            unsafe { state.withdraw_msi(self.virq) }
        };
        let Some((line, reservation, pending, retirements)) = withdrawn else {
            return false;
        };
        pending.retire();
        drop(reservation);
        crate::runtime::retirement::notify_retirements(self.virq, retirements);
        drop(line);
        true
    }

    /// No strong fixture reference hides the final callback release.
    pub fn is_reclaimed(&self) -> bool {
        self.owner.strong_count() == 0
    }
}

fn candidate(desc: IrqDesc) -> Arc<IrqLine> {
    let mut entry = IrqStateDesc::test_new(desc);
    entry.msi_reservation = Some(MsiReservation::for_test(desc.hwirq));
    entry.msi_withdrawal = Some(super::PreparedWithdrawal::new(desc.hwirq).unwrap());
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
        assert!(!entry.is_running);
    }
    // Reusing a logical number requires a fresh identity, not the old owner.
    let successor = candidate(desc);
    state.try_resolve_desc(desc, Some(&successor)).unwrap();
    assert!(Arc::ptr_eq(
        &state.descs.get(&desc.hwirq).unwrap(),
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
    assert!(Arc::ptr_eq(&state.descs.get(&virq).unwrap(), &existing));
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

#[def_test(serial)]
fn enabled_msi_without_actions_cannot_release_its_reservation() {
    let domain = crate::domain::TEST_DOMAIN;
    let desc = IrqDesc::from_hwirq(0x5fa3)
        .with_domain(domain)
        .with_flags(IrqFlags::MSI);
    let line = candidate(desc);
    let mut state = IrqState::new();
    let resolved = state.insert_msi_reservation(desc, &line).unwrap();
    let virq = resolved.into_desc().logical_irq().unwrap();
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
    assert!(Arc::ptr_eq(&state.descs.get(&virq).unwrap(), &line));
    assert_eq!(state.mapping_for_virq(virq), Some((domain, desc.hwirq)));
    assert_eq!(state.domain_virq(domain, desc.hwirq), Some(virq));
    assert_eq!(state.domain_virq(domain, desc.hwirq), Some(virq));
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
            registry.domain_virq(domain, desc.hwirq),
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
fn global_msi_allocation_release_withdraws_the_shared_domain_index() {
    // These probes use the global directory and the shared test-domain index.
    // Allocating and releasing them must not depend on a temporary index view
    // owning a population count: both entries belong to the domain itself.
    let first = RcuIndexProbe::new(0x5fc0, 0x5fc1);
    let second = RcuIndexProbe::new(0x5fc2, 0x5fc3);
    let domain = crate::domain::TEST_DOMAIN;
    assert_eq!(crate::domain::resolve(domain, 0x5fc1), Some(0x5fc0));
    assert_eq!(crate::domain::resolve(domain, 0x5fc3), Some(0x5fc2));

    assert!(first.retire());
    assert_eq!(crate::domain::resolve(domain, 0x5fc1), None);
    assert_eq!(crate::domain::resolve(domain, 0x5fc3), Some(0x5fc2));

    assert!(second.retire());
    assert_eq!(crate::domain::resolve(domain, 0x5fc3), None);

    // Reuse both number pairs through fresh identities after release.
    let first_again = RcuIndexProbe::new(0x5fc0, 0x5fc1);
    let second_again = RcuIndexProbe::new(0x5fc2, 0x5fc3);
    assert_eq!(crate::domain::resolve(domain, 0x5fc1), Some(0x5fc0));
    assert_eq!(crate::domain::resolve(domain, 0x5fc3), Some(0x5fc2));
    assert!(first_again.retire());
    assert!(second_again.retire());
}

#[def_test(serial)]
fn release_withdraws_both_indexes_before_returning_backend_ownership() {
    // Private indexes exercise real prepared withdrawal, without hardware I/O.
    let domain = crate::domain::TEST_DOMAIN;
    let desc = IrqDesc::from_hwirq(0x5fa2)
        .with_domain(domain)
        .with_flags(IrqFlags::MSI);
    let line = candidate(desc);
    let mut state = IrqState::new();
    let resolved = state.insert_msi_reservation(desc, &line).unwrap();
    let virq = resolved.into_desc().logical_irq().unwrap();
    assert_eq!(state.domain_virq(domain, desc.hwirq), Some(virq));
    let mut releases = 0;
    let mut observed = None;
    let removed = super::release_unused_msi(&mut state, virq, |registry, reservation| {
        observed = Some((
            registry.descs.contains_key(&virq),
            registry.mapping_for_virq(virq),
            registry.domain_virq(domain, desc.hwirq),
            reservation.hwirq(),
        ));
        releases += 1;
    })
    .unwrap();
    assert_eq!(observed, Some((false, None, None, desc.hwirq)));
    assert!(Arc::ptr_eq(&removed, &line));
    assert_eq!(releases, 1);
    assert!(super::release_unused_msi(&mut state, virq, |_, _| releases += 1).is_none());
    assert_eq!(releases, 1);
    assert!(line.lock().msi_reservation.is_none());
}


/// Covers the `withdraw_msi` primitive and its returned retirement queue.
/// The full `free_msi_if_unused` production wiring is covered separately below.
#[def_test]
fn msi_withdrawal_primitive_notifies_observer_after_index_removal() {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicUsize, Ordering};

    struct Observer(Arc<AtomicUsize>);
    impl crate::IrqLineRetirementObserver for Observer {
        fn on_line_retired(&self, _virq: crate::Virq) {
            self.0.fetch_add(1, Ordering::AcqRel);
        }
    }

    let probe = RcuIndexProbe::new(0x7e000079, 0x7e000079);
    let calls = Arc::new(AtomicUsize::new(0));
    let observer: Arc<dyn crate::IrqLineRetirementObserver> = Arc::new(Observer(calls.clone()));
    let handle = probe.handle().expect("current MSI handle");
    let subscription = handle
        .subscribe_retirement(Arc::downgrade(&observer))
        .expect("subscribe");
    assert!(probe.retire());
    assert_eq!(calls.load(Ordering::Acquire), 1);
    assert!(probe.handle().is_none());
    drop(subscription);
}


#[cfg(any(target_arch = "x86_64", unittest))]
#[def_test(serial)]
fn production_free_msi_notifies_after_index_and_backend_release() {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use crate::state::free_msi_if_unused;

    struct Observer {
        virq: crate::Virq,
        backend_released: Arc<AtomicBool>,
        index_removed_during_callback: Arc<AtomicBool>,
        calls: Arc<AtomicUsize>,
    }

    impl crate::IrqLineRetirementObserver for Observer {
        fn on_line_retired(&self, _virq: crate::Virq) {
            self.index_removed_during_callback.store(
                matches!(crate::lookup_irq_line(self.virq), Ok(None)),
                Ordering::Release,
            );
            core::assert!(self.backend_released.load(Ordering::Acquire));
            self.calls.fetch_add(1, Ordering::AcqRel);
        }
    }

    let virq = 0x7e00007a;
    let probe = RcuIndexProbe::new(virq, 0x7e00007a);
    let backend_released = Arc::new(AtomicBool::new(false));
    let index_removed_during_callback = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let observer: Arc<dyn crate::IrqLineRetirementObserver> = Arc::new(Observer {
        virq,
        backend_released: backend_released.clone(),
        index_removed_during_callback: index_removed_during_callback.clone(),
        calls: calls.clone(),
    });
    let handle = probe.handle().expect("current MSI handle");
    let subscription = handle
        .subscribe_retirement(Arc::downgrade(&observer))
        .expect("subscribe");

    let released = backend_released.clone();
    assert!(free_msi_if_unused(virq, move |_reservation| {
        released.store(true, Ordering::Release);
    }));
    assert_eq!(calls.load(Ordering::Acquire), 1);
    assert!(index_removed_during_callback.load(Ordering::Acquire));
    assert!(backend_released.load(Ordering::Acquire));
    assert!(matches!(crate::lookup_irq_line(virq), Ok(None)));
    drop(subscription);
}

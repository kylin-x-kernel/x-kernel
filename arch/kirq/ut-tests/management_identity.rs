// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use unittest::{assert, assert_eq, assert_ne, def_test};

use super::{IRQ_STATE, IrqLine, IrqStateDesc};
use crate::runtime::manager::take_from_line;
use crate::{Handler, IrqDesc, IrqDescError, IrqEvent};

struct ReplaceBeforeAdmission {
    virq: usize,
    original: Arc<IrqLine>,
    replacement: Arc<IrqLine>,
}

// Serial fixture tests exclusively own a vacant numeric slot, including any
// successor or mapping they publish there. Never arm cleanup for an old Line.
struct ScopedTestLine {
    virq: usize,
}

impl ScopedTestLine {
    fn for_vacant_slot(virq: usize) -> Option<Self> {
        if IRQ_STATE.lock().descs.contains_key(&virq) {
            return None;
        }
        Some(Self { virq })
    }
}

impl Drop for ScopedTestLine {
    fn drop(&mut self) {
        let removed = { IRQ_STATE.lock().remove_test_line(self.virq) };
        // Handler destructors may reenter management; run them after unlocking.
        drop(removed);
    }
}

#[cfg(target_arch = "aarch64")]
#[def_test(serial)]
fn invalid_gic_source_is_rejected_before_mapping_or_action_publication() {
    let virq = crate::DYNAMIC_VIRQ_BASE + 0x5f76;
    let cleanup = ScopedTestLine::for_vacant_slot(virq);
    assert!(cleanup.is_some());
    let handler: Handler = Arc::new(|_| IrqEvent::HANDLED);
    for hwirq in [1020, 1023, 1024, 8192, u32::MAX as usize + 1, usize::MAX] {
        let desc = crate::gic_level_irq_desc(hwirq).with_virq(virq);
        let expected = IrqDescError::UnsupportedFlow { hwirq };
        // If the provider gate regresses, fail before registration could reach
        // hardware with a reserved or truncated INTID.
        assert_eq!(crate::IntrManagerIf::select_flow(desc), Err(expected));
        assert_eq!(super::try_map(desc), Err(expected));
        assert_eq!(
            super::try_register_disabled(desc, handler.clone()),
            Err(IrqDescError::UnknownIrq)
        );
        assert!(crate::descriptor(virq).is_none());
        assert!(
            IRQ_STATE
                .lock()
                .translated_hwirq(crate::GIC_ROOT_DOMAIN, hwirq)
                .is_none()
        );
        assert_eq!(Arc::strong_count(&handler), 1);
    }
}

#[cfg(target_arch = "aarch64")]
#[def_test]
fn unsupported_gic_spi_trigger_preserves_resolution_state() {
    let hwirq = 32;
    let virq = crate::DYNAMIC_VIRQ_BASE + 0x5f77;
    let original = crate::gic_level_irq_desc(hwirq).with_virq(virq);
    assert_eq!(
        matches!(
            crate::IntrManagerIf::select_flow(original),
            Ok(crate::IrqFlow::Fasteoi(_))
        ),
        true
    );
    for trigger in [crate::IrqTrigger::EdgeFalling, crate::IrqTrigger::LevelLow] {
        for is_existing in [false, true] {
            // A private registry exercises real resolution without publishing
            // a hardware mapping or touching the controller's live source 32.
            let mut state = super::IrqState::new();
            let line = Arc::new(IrqLine::new(IrqStateDesc::new(original)));
            if is_existing {
                state.descs.insert(virq, line.clone());
            }
            let snapshot = |entry: &IrqStateDesc| {
                (
                    entry.desc,
                    entry.generation_for_tests(),
                    entry.configured_generation_for_tests(),
                    entry.action_count(),
                    entry.disable_depth_for_tests(),
                    entry.is_enabled_for_tests(),
                )
            };
            let before = snapshot(&line.lock());
            let offered = crate::gic_irq_desc(hwirq, trigger).with_virq(virq);
            let result = state.try_resolve_desc(offered, Some(&line));
            assert!(
                matches!(result, Err(IrqDescError::UnsupportedFlow { hwirq: id }) if id == hwirq)
            );
            assert_eq!(state.descs.contains_key(&virq), is_existing);
            assert!(
                state
                    .translated_hwirq(crate::GIC_ROOT_DOMAIN, hwirq)
                    .is_none()
            );
            assert_eq!(snapshot(&line.lock()), before);
        }
    }
}

// These Lines are software-only fixtures. Rejected richer descriptors must
// never reach platform configuration or publish their offered domain mapping.
enum RegistrationRejection {
    DuplicateRegular,
    IncompatibleShared,
    ExhaustedSequence,
}

#[expect(
    clippy::unwrap_used,
    reason = "CG-021-no-panic-on-reachable-failure: test scenario: valid fixture requests must \
              return policy rejection and retain the original registered Line"
)]
fn check_registration_rejection(
    virq: usize,
    rejection: RegistrationRejection,
) -> unittest::TestResult {
    let hwirq = crate::DYNAMIC_VIRQ_BASE + virq;
    let desc = IrqDesc::from_hwirq(hwirq).with_virq(virq);
    let richer = desc
        .with_domain(crate::GIC_ROOT_DOMAIN)
        .with_controller(crate::IrqController::Gic)
        .with_source(crate::IrqSource::Acpi);
    let installed: Handler = Arc::new(|_| IrqEvent::HANDLED);
    let offered: Handler = Arc::new(|_| IrqEvent::HANDLED);
    let mut entry = IrqStateDesc::new(desc);
    if matches!(rejection, RegistrationRejection::ExhaustedSequence) {
        assert!(entry.install_shared_action(installed.clone()).is_some());
        entry.exhaust_action_sequences_for_tests();
    } else {
        assert!(
            entry.install_regular_action(crate::action::IrqAction::regular(installed.clone(),))
        );
    }
    let snapshot = |entry: &IrqStateDesc| {
        (
            entry.desc,
            entry.generation_for_tests(),
            entry.configured_generation_for_tests(),
            entry.action_count(),
            entry.disable_depth_for_tests(),
            entry.is_enabled_for_tests(),
        )
    };
    let before = snapshot(&entry);
    let owners_before = Arc::strong_count(&installed);
    let line = Arc::new(IrqLine::new(entry));
    assert!(
        IRQ_STATE
            .lock()
            .translated_hwirq(crate::GIC_ROOT_DOMAIN, hwirq)
            .is_none()
    );
    let cleanup = ScopedTestLine::for_vacant_slot(virq);
    assert!(cleanup.is_some());
    {
        let mut state = IRQ_STATE.lock();
        assert!(state.descs.insert(virq, line.clone()).is_none());
    }
    let rejected = if !matches!(rejection, RegistrationRejection::DuplicateRegular) {
        super::try_register_shared(richer, offered.clone())
            .unwrap()
            .is_none()
    } else {
        let task_rejected = !super::try_register(richer, offered.clone()).unwrap();
        let startup_rejected = !super::try_register_startup(
            richer,
            offered.clone(),
            super::IrqRegisterOptions::auto_enable(),
        )
        .unwrap();
        task_rejected && startup_rejected
    };
    let after = snapshot(&line.lock());
    let installed_owners = Arc::strong_count(&installed);
    let offered_owners = Arc::strong_count(&offered);
    let (has_no_mapping, removed) = {
        let mut state = IRQ_STATE.lock();
        (
            state
                .translated_hwirq(crate::GIC_ROOT_DOMAIN, hwirq)
                .is_none(),
            state.remove_test_line(virq),
        )
    };
    assert!(Arc::ptr_eq(removed.as_ref().unwrap(), &line));
    drop(removed);
    drop(line);
    drop(cleanup);
    assert!(rejected);
    assert_eq!(after, before);
    assert!(has_no_mapping);
    assert_eq!(installed_owners, owners_before);
    assert_eq!(offered_owners, 1);
    assert_eq!(Arc::strong_count(&installed), 1);
    unittest::TestResult::Ok
}

#[def_test(serial)]
fn duplicate_regular_registration_preserves_richer_legacy_metadata_and_owners()
-> unittest::TestResult {
    check_registration_rejection(0x5f70, RegistrationRejection::DuplicateRegular)
}

#[def_test(serial)]
fn incompatible_shared_registration_preserves_richer_legacy_metadata_and_owners()
-> unittest::TestResult {
    check_registration_rejection(0x5f71, RegistrationRejection::IncompatibleShared)
}

#[def_test(serial)]
fn exhausted_sequence_registration_preserves_richer_legacy_metadata_and_owners()
-> unittest::TestResult {
    check_registration_rejection(0x5f72, RegistrationRejection::ExhaustedSequence)
}

#[def_test(serial)]
fn registration_preflight_preserves_regular_and_shared_success() {
    // Metadata-free synthetic sources bypass platform register access.
    let regular = crate::DYNAMIC_VIRQ_BASE + 0x5f73;
    let shared = crate::DYNAMIC_VIRQ_BASE + 0x5f74;
    let regular_cleanup = ScopedTestLine::for_vacant_slot(regular);
    assert!(regular_cleanup.is_some());
    let shared_cleanup = ScopedTestLine::for_vacant_slot(shared);
    assert!(shared_cleanup.is_some());
    let handler: Handler = Arc::new(|_| IrqEvent::HANDLED);
    assert_eq!(super::try_map(IrqDesc::from_virq(regular)), Ok(regular));
    assert_eq!(super::try_map(IrqDesc::from_virq(shared)), Ok(shared));
    assert!(super::try_register_disabled(regular, handler.clone()).unwrap());
    {
        let state = IRQ_STATE.lock();
        let retained_line = state.descs.get(&regular).unwrap();
        let entry = retained_line.lock();
        assert_eq!(entry.action_count(), 1);
        assert_eq!(entry.disable_depth_for_tests(), 1);
        assert!(!entry.is_enabled_for_tests());
    }
    assert!(super::try_enable_irq(regular).is_ok());
    assert!(super::unregister(regular).is_some());
    let first = super::try_register_shared(shared, handler.clone())
        .unwrap()
        .unwrap();
    let second = super::try_register_shared(shared, handler.clone())
        .unwrap()
        .unwrap();
    assert_ne!(first, second);
    {
        let state = IRQ_STATE.lock();
        let retained_line = state.descs.get(&shared).unwrap();
        let entry = retained_line.lock();
        assert_eq!(entry.action_count(), 2);
        assert!(entry.is_enabled_for_tests());
        assert_eq!(entry.disable_depth_for_tests(), 0);
    }
    assert!(super::free_irq_action(shared, first).is_some());
    assert!(super::free_irq_action(shared, second).is_some());
    assert_eq!(Arc::strong_count(&handler), 1);
    let mut state = IRQ_STATE.lock();
    assert!(state.remove_if_unused(regular).is_some());
    assert!(state.remove_if_unused(shared).is_some());
}

#[def_test(serial)]
fn failed_registration_retains_resolved_metadata_without_committing_configuration() {
    let virq = crate::DYNAMIC_VIRQ_BASE + 0x5f78;
    let hwirq = 0x5f78;
    let domain = crate::domain::TEST_DOMAIN;
    let desc = IrqDesc::from_hwirq(hwirq).with_virq(virq);
    let offered = desc
        .with_domain(domain)
        .with_flags(crate::IrqFlags::WAKEUP_SOURCE);
    let handler: Handler = Arc::new(|_| IrqEvent::HANDLED);
    for kind in 0..3 {
        let cleanup = ScopedTestLine::for_vacant_slot(virq);
        assert!(cleanup.is_some());
        assert!(IRQ_STATE.lock().translated_hwirq(domain, hwirq).is_none());
        let line = Arc::new(IrqLine::new(IrqStateDesc::new(desc)));
        assert!(IRQ_STATE.lock().descs.insert(virq, line.clone()).is_none());
        let generation_before = line.lock().generation_for_tests();
        let irq_before = karch::local_irq_enabled();
        let mut observed = None;
        let result = super::with_registration_line_checked(
            offered.into(),
            super::ManagementMode::Task,
            |entry| {
                Ok(if kind == 2 {
                    entry.can_install_shared_action()
                } else {
                    entry.can_install_regular_action()
                })
            },
            |resolved, _, entry| {
                let fail = |plan: super::IrqPlatformPlan| {
                    observed = Some((
                        plan.desc,
                        plan.configure,
                        plan.enable,
                        plan.flow_unmask.is_some(),
                        Arc::strong_count(&handler),
                    ));
                    Err(IrqDescError::ConfigurationFailed { hwirq })
                };
                if kind == 2 {
                    super::register_shared_locked(resolved, entry, &handler, fail)
                        .map(|token| token.is_some())
                } else {
                    let options = if kind == 0 {
                        super::IrqRegisterOptions::auto_enable()
                    } else {
                        super::IrqRegisterOptions::disabled()
                    };
                    super::register_locked(resolved, entry, &handler, options, fail)
                }
            },
        );
        assert_eq!(result, Err(IrqDescError::ConfigurationFailed { hwirq }));
        // The unpublished PreparedAction retains the offered handler through
        // configuration; failure returns that extra owner before this call ends.
        assert_eq!(observed, Some((offered, true, None, false, 2)));
        assert_eq!(karch::local_irq_enabled(), irq_before);
        assert_eq!(IRQ_STATE.lock().translated_hwirq(domain, hwirq), Some(virq));
        {
            let entry = line.lock();
            assert_eq!(entry.desc, offered);
            assert!(entry.generation_for_tests() > generation_before);
            assert_eq!(entry.configured_generation_for_tests(), None);
            assert_eq!(entry.action_count(), 0);
            assert_eq!(entry.disable_depth_for_tests(), 0);
            assert!(!entry.is_enabled_for_tests());
        }
        assert_eq!(Arc::strong_count(&handler), 1);
        drop(cleanup);
        assert!(IRQ_STATE.lock().translated_hwirq(domain, hwirq).is_none());
    }
}

#[def_test(serial)]
fn failed_legacy_enable_retains_prepared_mapping_without_enabling_line() {
    let virq = crate::DYNAMIC_VIRQ_BASE + 0x5f79;
    let hwirq = 0x5f79;
    let domain = crate::domain::TEST_DOMAIN;
    let desc = IrqDesc::from_hwirq(hwirq)
        .with_virq(virq)
        .with_domain(domain);
    let cleanup = ScopedTestLine::for_vacant_slot(virq);
    assert!(cleanup.is_some());
    assert!(IRQ_STATE.lock().translated_hwirq(domain, hwirq).is_none());
    assert_eq!(super::try_map(desc), Ok(virq));
    let prepared = IRQ_STATE.lock().descs.get(&virq).unwrap();
    let irq_before = karch::local_irq_enabled();
    let mut observed = None;
    let result = super::enable_line(&IRQ_STATE, desc.into(), true, |plan| {
        observed = Some((plan.desc, plan.configure, plan.enable));
        Err(IrqDescError::ConfigurationFailed { hwirq })
    });
    assert_eq!(result, Err(IrqDescError::ConfigurationFailed { hwirq }));
    assert_eq!(observed, Some((desc, true, Some(true))));
    assert_eq!(karch::local_irq_enabled(), irq_before);
    assert_eq!(IRQ_STATE.lock().translated_hwirq(domain, hwirq), Some(virq));
    let line = IRQ_STATE.lock().descs.get(&virq).unwrap();
    assert!(Arc::ptr_eq(&line, &prepared));
    let entry = line.lock();
    assert_eq!(entry.desc, desc);
    assert_eq!(entry.configured_generation_for_tests(), None);
    assert_eq!(entry.action_count(), 0);
    assert_eq!(entry.disable_depth_for_tests(), 0);
    assert!(!entry.is_enabled_for_tests());
}

#[def_test(serial)]
fn first_action_registration_unmasks_an_already_enabled_line() {
    static IS_MASKED: AtomicBool = AtomicBool::new(false);
    static CHIP: crate::FasteoiChip = crate::FasteoiChip {
        mask: |_| IS_MASKED.store(true, Ordering::Relaxed),
        resend: crate::FasteoiResend::Software,
        unmask: |_| IS_MASKED.store(false, Ordering::Relaxed),
    };
    let virq = 0x5f49;
    let mut entry = IrqStateDesc::new(IrqDesc::from_virq(virq));
    entry.flow = crate::IrqFlow::Fasteoi(&CHIP);
    let plan = entry.prepare_legacy_enable();
    entry.commit_platform_plan(plan);
    (CHIP.mask)(entry.desc);
    entry.is_pending = true;
    let handler: Handler = Arc::new(|_| IrqEvent::HANDLED);
    assert!(
        super::register_locked(
            virq,
            &mut entry,
            &handler,
            super::IrqRegisterOptions::auto_enable(),
            |plan| {
                core::assert_eq!(plan.enable, None);
                super::apply_platform_plan(plan)
            },
        )
        .unwrap()
    );
    assert!(!IS_MASKED.load(Ordering::Relaxed));
    assert!(entry.is_pending);
}

#[def_test(serial)]
fn shared_withdrawal_restores_pending_service_after_teardown() {
    static HAS_UNMASKED: AtomicBool = AtomicBool::new(false);
    static CHIP: crate::FasteoiChip = crate::FasteoiChip {
        mask: |_| {},
        unmask: |_| HAS_UNMASKED.store(true, Ordering::Release),
        resend: crate::FasteoiResend::Software,
    };
    HAS_UNMASKED.store(false, Ordering::Relaxed);
    let calls = Arc::new(core::sync::atomic::AtomicUsize::new(0));
    let count = calls.clone();
    let mut entry = IrqStateDesc::new(IrqDesc::from_virq(0x5f46));
    entry.flow = crate::IrqFlow::Fasteoi(&CHIP);
    let removed_token = entry
        .install_shared_action(Arc::new(|_| IrqEvent::HANDLED))
        .unwrap();
    entry
        .install_shared_action(Arc::new(move |_| {
            core::assert!(HAS_UNMASKED.load(Ordering::Acquire));
            count.fetch_add(1, Ordering::Relaxed);
            IrqEvent::HANDLED
        }))
        .unwrap();
    let plan = entry.prepare_auto_enable();
    entry.commit_platform_plan(plan);
    let line = Arc::new(IrqLine::new(entry));
    {
        let mut entry = line.lock();
        entry.begin_management();
        entry.begin_teardown();
        entry.is_pending = true;
    }
    let removed = take_from_line(&line, Some(removed_token));
    assert!(removed.is_some());
    let completion = line.lock().in_flight_zero_completion();
    drop(super::ManagementAdmission {
        line: &line,
        is_active: true,
        has_removed_action: true,
    });
    kiface_task_wait_api::wait_for_completion(&completion).unwrap();
    assert!(HAS_UNMASKED.load(Ordering::Acquire));
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(line.lock().action_count(), 1);
    assert!(!line.lock().is_teardown_in_progress());
    drop(removed);
}

#[def_test(serial)]
fn shared_edge_withdrawal_unmasks_before_requesting_hardware_delivery() {
    static HAS_UNMASKED: AtomicBool = AtomicBool::new(false);
    static HAS_REQUESTED: AtomicBool = AtomicBool::new(false);
    static CHIP: crate::EdgeChip = crate::EdgeChip {
        mask: |_| {},
        unmask: |_| HAS_UNMASKED.store(true, Ordering::Relaxed),
        retrigger: |_| {
            core::assert!(HAS_UNMASKED.load(Ordering::Relaxed));
            HAS_REQUESTED.store(true, Ordering::Relaxed);
            true
        },
    };
    HAS_UNMASKED.store(false, Ordering::Relaxed);
    HAS_REQUESTED.store(false, Ordering::Relaxed);
    let mut entry = IrqStateDesc::new(IrqDesc::from_virq(0x5f46));
    entry.flow = crate::IrqFlow::Edge(&CHIP);
    let token = entry
        .install_shared_action(Arc::new(|_| IrqEvent::HANDLED))
        .unwrap();
    entry
        .install_shared_action(Arc::new(|_| -> IrqEvent {
            panic!("hardware request must not run actions inline")
        }))
        .unwrap();
    let plan = entry.prepare_auto_enable();
    entry.commit_platform_plan(plan);
    let line = Arc::new(IrqLine::new(entry));
    {
        let mut entry = line.lock();
        entry.begin_management();
        entry.begin_teardown();
        entry.is_pending = true;
    }
    let removed = take_from_line(&line, Some(token));
    assert!(removed.is_some());
    drop(super::ManagementAdmission {
        line: &line,
        is_active: true,
        has_removed_action: true,
    });
    assert!(HAS_REQUESTED.load(Ordering::Relaxed));
    assert!(!line.lock().is_pending);
    assert_eq!(line.lock().action_count(), 1);
    assert_eq!(line.lock().in_flight(), 0);
    assert!(!line.lock().is_teardown_in_progress());
    drop(removed);
}

#[def_test(serial)]
fn only_final_enable_replays_an_unknown_trigger_once() {
    static CHIP: crate::FasteoiChip = crate::FasteoiChip {
        mask: |_| {},
        unmask: |_| {},
        resend: crate::FasteoiResend::Software,
    };
    let virq = 0x5f48;
    let calls = Arc::new(core::sync::atomic::AtomicUsize::new(0));
    let count = calls.clone();
    let mut entry = IrqStateDesc::new(IrqDesc::from_virq(virq));
    entry.flow = crate::IrqFlow::Fasteoi(&CHIP);
    entry.install_regular_action(crate::action::IrqAction::regular(Arc::new(move |_| {
        count.fetch_add(1, Ordering::Relaxed);
        IrqEvent::HANDLED
    })));
    let plan = entry.prepare_register_disabled();
    entry.commit_platform_plan(plan);
    let plan = entry.prepare_disable_irq_nosync();
    entry.commit_platform_plan(plan);
    entry.is_pending = true;
    let line = Arc::new(IrqLine::new(entry));
    let registry = kspin::SpinNoIrq::new(super::IrqState::new());
    registry.lock().descs.insert(virq, line.clone());
    let completion = line.lock().in_flight_zero_completion();
    super::enable_line(&registry, virq.into(), false, |_| Ok(())).unwrap();
    assert!(line.lock().is_pending);
    assert_eq!(line.lock().in_flight(), 0);
    super::enable_line(&registry, virq.into(), false, |_| Ok(())).unwrap();
    super::enable_line(&registry, virq.into(), false, |_| Ok(())).unwrap();
    kiface_task_wait_api::wait_for_completion(&completion).unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert!(!line.lock().is_pending);
}

kspin::static_lock! {
    static REPLACE_BEFORE_ADMISSION: kspin::SpinNoIrq<Option<ReplaceBeforeAdmission>> =
        kspin::SpinNoIrq::new(None);
}

kspin::static_lock! {
    static WITHDRAW_BEFORE_ADMISSION: kspin::SpinNoIrq<Option<Arc<IrqLine>>> =
        kspin::SpinNoIrq::new(None);
}

fn withdraw_before_admission(line: &Arc<IrqLine>) {
    let withdrawn = {
        let mut pending = WITHDRAW_BEFORE_ADMISSION.lock();
        if pending
            .as_ref()
            .is_some_and(|selected| Arc::ptr_eq(selected, line))
        {
            pending.take()
        } else {
            None
        }
    };
    if let Some(withdrawn) = withdrawn {
        let virq = withdrawn.lock().desc.logical_irq().unwrap();
        let removed = { IRQ_STATE.lock().remove_test_line(virq) };
        drop(removed);
    }
}

/// Retires the selected identity before acquiring the real mutex, once only.
pub(super) fn replace_before_management_acquire(line: &Arc<IrqLine>) {
    withdraw_before_admission(line);
    let replacement = {
        let mut pending = REPLACE_BEFORE_ADMISSION.lock();
        if pending
            .as_ref()
            .is_some_and(|hook| Arc::ptr_eq(&hook.original, line))
        {
            pending.take()
        } else {
            None
        }
    };
    if let Some(replacement) = replacement {
        let removed = IRQ_STATE
            .lock()
            .descs
            .insert(replacement.virq, replacement.replacement);
        drop(removed);
    }
}

#[def_test(serial)]
fn registration_does_not_recreate_line_withdrawn_before_admission() {
    let virq = 0x5f56;
    let cleanup = ScopedTestLine::for_vacant_slot(virq);
    assert!(cleanup.is_some());
    assert_eq!(crate::try_map(IrqDesc::from_virq(virq)), Ok(virq));
    let original = IRQ_STATE.lock().descs.get(&virq).unwrap();
    *WITHDRAW_BEFORE_ADMISSION.lock() = Some(original.clone());
    let handler: Handler = Arc::new(|_| IrqEvent::HANDLED);
    let registered = crate::try_register_disabled(virq, handler.clone());
    let pending = WITHDRAW_BEFORE_ADMISSION.lock().take();

    assert_eq!(registered, Err(IrqDescError::UnknownIrq));
    assert!(pending.is_none());
    assert!(crate::descriptor(virq).is_none());
    assert_eq!(original.lock().action_count(), 0);
    assert!(original.lock().is_unused());
    assert!(original.management.try_lock().is_some());
    assert_eq!(Arc::strong_count(&handler), 1);
}

struct ScopedReplacement {
    original: Arc<IrqLine>,
}

impl Drop for ScopedReplacement {
    fn drop(&mut self) {
        let pending = {
            let mut pending = REPLACE_BEFORE_ADMISSION.lock();
            if pending
                .as_ref()
                .is_some_and(|hook| Arc::ptr_eq(&hook.original, &self.original))
            {
                pending.take()
            } else {
                None
            }
        };
        drop(pending);
    }
}

fn publish_replaced_on_acquisition(
    virq: usize,
    replacement: &Arc<IrqLine>,
) -> (Arc<IrqLine>, ScopedReplacement) {
    let original = Arc::new(IrqLine::new(IrqStateDesc::new(IrqDesc::from_virq(virq))));
    let hook = ReplaceBeforeAdmission {
        virq,
        original: original.clone(),
        replacement: replacement.clone(),
    };
    core::assert!(
        IRQ_STATE
            .lock()
            .descs
            .insert(virq, original.clone())
            .is_none()
    );
    {
        let mut pending = REPLACE_BEFORE_ADMISSION.lock();
        core::assert!(pending.is_none());
        *pending = Some(hook);
    }
    let scope = ScopedReplacement {
        original: original.clone(),
    };
    (original, scope)
}

#[def_test(serial)]
fn stale_removal_does_not_remove_replacement_action() {
    let virq = 0x5f50;
    let cleanup = ScopedTestLine::for_vacant_slot(virq);
    assert!(cleanup.is_some());
    let handler: Handler = Arc::new(|_| IrqEvent::HANDLED);
    let mut state = IrqStateDesc::new(IrqDesc::from_virq(virq));
    assert!(state.install_regular_action(crate::action::IrqAction::regular(handler.clone())));
    let replacement = Arc::new(IrqLine::new(state));
    let (original, _replacement_hook) = publish_replaced_on_acquisition(virq, &replacement);

    let removed = crate::try_free_irq(virq).unwrap();
    let count = replacement.lock().action_count();
    let remains_original = original.lock().action_count();
    let removed_replacement = crate::try_free_irq(virq).unwrap();

    assert!(removed.is_none());
    assert_eq!(count, 1);
    assert_eq!(remains_original, 0);
    assert!(removed_replacement.is_some());
    drop(removed_replacement);
    assert_eq!(Arc::strong_count(&handler), 1);
}

#[def_test(serial)]
fn stale_registration_retries_current_line() {
    let virq = 0x5f51;
    let cleanup = ScopedTestLine::for_vacant_slot(virq);
    assert!(cleanup.is_some());
    let replacement = Arc::new(IrqLine::new(IrqStateDesc::new(IrqDesc::from_virq(virq))));
    let (original, _replacement_hook) = publish_replaced_on_acquisition(virq, &replacement);
    let registered = crate::try_register_disabled(virq, Arc::new(|_| IrqEvent::HANDLED));
    let original_count = original.lock().action_count();
    let replacement_count = replacement.lock().action_count();
    let removed = crate::try_free_irq(virq).unwrap();

    assert_eq!(registered, Ok(true));
    assert_eq!(original_count, 0);
    assert_eq!(replacement_count, 1);
    assert!(removed.is_some());
}

#[def_test(serial)]
fn stale_registration_rechecks_replacement_metadata() {
    let virq = 0x5f52;
    let cleanup = ScopedTestLine::for_vacant_slot(virq);
    assert!(cleanup.is_some());
    let replacement_desc = IrqDesc::from_hwirq(0x5f53).with_virq(virq);
    let replacement = Arc::new(IrqLine::new(IrqStateDesc::new(replacement_desc)));
    let (original, _replacement_hook) = publish_replaced_on_acquisition(virq, &replacement);
    let irq_enabled_before = karch::local_irq_enabled();
    let registered =
        crate::try_register_disabled(IrqDesc::from_virq(virq), Arc::new(|_| IrqEvent::HANDLED));
    let irq_enabled_after = karch::local_irq_enabled();
    let (desc, count, is_unused) = {
        let entry = replacement.lock();
        (entry.desc, entry.action_count(), entry.is_unused())
    };
    let is_management_unlocked = replacement.management.try_lock().is_some();
    // Startup acquisition is nonblocking: a leaked management mutex must
    // produce a failed retry rather than hang the test on a second lock().
    let retried = if is_unused {
        crate::try_register_startup(
            replacement_desc,
            Arc::new(|_| IrqEvent::HANDLED),
            super::IrqRegisterOptions::disabled(),
        )
    } else {
        Ok(false)
    };
    let replacement_count = replacement.lock().action_count();
    let removed = IRQ_STATE.lock().remove_test_line(virq);
    let has_same_identity = removed
        .as_ref()
        .is_some_and(|line| Arc::ptr_eq(line, &replacement));
    drop(removed);

    assert!(matches!(
        registered,
        Err(IrqDescError::HwirqConflict { .. })
    ));
    assert_eq!(desc, replacement_desc);
    assert_eq!(count, 0);
    assert_eq!(irq_enabled_after, irq_enabled_before);
    assert!(is_unused);
    assert!(is_management_unlocked);
    assert_eq!(retried, Ok(true));
    assert!(has_same_identity);
    assert_eq!(replacement_count, 1);
    assert_eq!(original.lock().action_count(), 0);
}

#[def_test(serial)]
fn unmatched_free_preserves_empty_line_control_state() {
    // Metadata-free synthetic sources bypass backend MMIO even on regression.
    let virq = crate::DYNAMIC_VIRQ_BASE + 0x5f75;
    let cleanup = ScopedTestLine::for_vacant_slot(virq);
    assert!(cleanup.is_some());
    for is_enabled in [false, true] {
        for token in [None, Some(crate::IrqActionToken::new(1))] {
            let mut entry = IrqStateDesc::new(IrqDesc::from_virq(virq));
            if is_enabled {
                // Commit the software-only fixture's prepared enable state.
                let plan = entry.prepare_legacy_enable();
                entry.commit_platform_plan(plan);
            }
            let snapshot = |entry: &IrqStateDesc| {
                (
                    entry.desc,
                    entry.is_enabled_for_tests(),
                    entry.disable_depth_for_tests(),
                    entry.teardown_depth_for_tests(),
                    entry.action_count(),
                    entry.generation_for_tests(),
                    entry.configured_generation_for_tests(),
                )
            };
            let before = snapshot(&entry);
            let line = Arc::new(IrqLine::new(entry));
            assert!(IRQ_STATE.lock().descs.insert(virq, line.clone()).is_none());
            let result = match token {
                Some(token) => crate::try_free_irq_action(virq, token),
                None => crate::try_free_irq(virq),
            };
            let after = snapshot(&line.lock());
            let removed = IRQ_STATE.lock().remove_test_line(virq);
            let has_same_identity = removed
                .as_ref()
                .is_some_and(|stored| Arc::ptr_eq(stored, &line));
            drop(removed);
            assert!(matches!(result, Ok(None)));
            assert!(has_same_identity);
            assert_eq!(after, before);
            assert!(line.management.try_lock().is_some());
        }
    }
    assert!(matches!(crate::try_free_irq(virq), Ok(None)));
    assert!(matches!(
        crate::try_free_irq_action(virq, crate::IrqActionToken::new(1)),
        Ok(None)
    ));
    assert!(crate::descriptor(virq).is_none());
}

struct ReentrantDrop {
    line: Arc<IrqLine>,
    has_unlocked_drop: Arc<AtomicBool>,
}

impl crate::IrqHandler for ReentrantDrop {
    fn handle(&self, _irq: usize) -> IrqEvent {
        IrqEvent::HANDLED
    }
}

impl Drop for ReentrantDrop {
    fn drop(&mut self) {
        // A leaked local SpinNoIrq guard leaves IRQs masked. Check before
        // reentry to avoid recursive deadlock, then tolerate remote timer users.
        let has_registry_access = if karch::local_irq_enabled() {
            drop(IRQ_STATE.lock());
            true
        } else {
            false
        };
        let has_line_access = self.line.management.try_lock().is_some();
        self.has_unlocked_drop
            .store(has_registry_access && has_line_access, Ordering::Release);
    }
}

struct CountedReentrantDrop {
    reentry: ReentrantDrop,
    drops: Arc<AtomicUsize>,
}

impl crate::IrqHandler for CountedReentrantDrop {
    fn handle(&self, irq: usize) -> IrqEvent {
        self.reentry.handle(irq)
    }
}

impl Drop for CountedReentrantDrop {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::Relaxed);
        // Field destruction then exercises ReentrantDrop's same-Line reentry.
    }
}

#[def_test(serial)]
fn shared_public_api_churn_reclaims_each_owner_after_unlock() {
    // Metadata-free synthetic identity: public registration performs no MMIO.
    let virq = crate::DYNAMIC_VIRQ_BASE + 0x5fb8;
    let cleanup = ScopedTestLine::for_vacant_slot(virq);
    assert!(cleanup.is_some());
    assert_eq!(crate::try_map(IrqDesc::from_virq(virq)), Ok(virq));
    let line = IRQ_STATE.lock().descs.get(&virq).unwrap().clone();
    let mut previous_token = 0;
    for _ in 0..8 {
        let drops: [_; 6] = core::array::from_fn(|_| Arc::new(AtomicUsize::new(0)));
        let unlocked: [_; 6] = core::array::from_fn(|_| Arc::new(AtomicBool::new(false)));
        let mut tokens = [None; 6];
        let mut owners = alloc::vec::Vec::new();
        for index in 0..6 {
            let handler: Handler = Arc::new(CountedReentrantDrop {
                reentry: ReentrantDrop {
                    line: line.clone(),
                    has_unlocked_drop: unlocked[index].clone(),
                },
                drops: drops[index].clone(),
            });
            owners.push(Arc::downgrade(&handler));
            let token = crate::try_register_shared(virq, handler).unwrap().unwrap();
            assert!(token.id() > previous_token);
            previous_token = token.id();
            tokens[index] = Some(token);
        }
        assert_eq!(line.lock().action_count(), 6);
        // Alternate middle positions, then remove the two endpoints.
        for (removed_count, index) in [2, 3, 1, 4, 0, 5].into_iter().enumerate() {
            let token = tokens[index].take().unwrap();
            let removed = crate::try_free_irq_action(virq, token).unwrap().unwrap();
            assert_eq!(Arc::strong_count(&removed), 1);
            assert_eq!(drops[index].load(Ordering::Relaxed), 0);
            assert!(!unlocked[index].load(Ordering::Acquire));
            assert!(!line.lock().is_teardown_in_progress());
            assert_eq!(line.lock().action_count(), 5 - removed_count);
            drop(removed);
            assert_eq!(drops[index].load(Ordering::Relaxed), 1);
            assert!(unlocked[index].load(Ordering::Acquire));
            assert!(owners[index].upgrade().is_none());
            assert!(crate::try_free_irq_action(virq, token).unwrap().is_none());
        }
        for count in &drops {
            assert_eq!(count.load(Ordering::Relaxed), 1);
        }
        assert!(line.management.try_lock().is_some());
        assert!(line.lock().is_unused());
    }
}

#[def_test(serial)]
fn rejected_handler_destructor_runs_after_management_unlock() {
    let virq = 0x5f54;
    let cleanup = ScopedTestLine::for_vacant_slot(virq);
    assert!(cleanup.is_some());
    assert_eq!(crate::try_map(IrqDesc::from_virq(virq)), Ok(virq));
    assert_eq!(
        crate::try_register_disabled(virq, Arc::new(|_| IrqEvent::HANDLED)),
        Ok(true)
    );
    let line = IRQ_STATE.lock().descs.get(&virq).unwrap().clone();
    let has_unlocked_drop = Arc::new(AtomicBool::new(false));
    let rejected = crate::try_register_disabled(
        virq,
        Arc::new(ReentrantDrop {
            line,
            has_unlocked_drop: has_unlocked_drop.clone(),
        }),
    );
    let removed = crate::try_free_irq(virq).unwrap();

    assert_eq!(rejected, Ok(false));
    assert!(has_unlocked_drop.load(Ordering::Acquire));
    assert!(removed.is_some());
}

#[def_test(serial)]
fn removed_handler_destructor_runs_after_management_cleanup() {
    let virq = 0x5f59;
    let cleanup = ScopedTestLine::for_vacant_slot(virq);
    assert!(cleanup.is_some());
    let line = Arc::new(IrqLine::new(IrqStateDesc::new(IrqDesc::from_virq(virq))));
    let has_unlocked_drop = Arc::new(AtomicBool::new(false));
    let handler: Handler = Arc::new(ReentrantDrop {
        line: line.clone(),
        has_unlocked_drop: has_unlocked_drop.clone(),
    });
    // A synthetic disabled Line avoids touching any real controller source.
    assert!(
        line.lock()
            .install_regular_action(crate::action::IrqAction::regular(handler))
    );
    assert!(IRQ_STATE.lock().descs.insert(virq, line.clone()).is_none());
    let removed = crate::try_free_irq(virq).unwrap();
    assert!(removed.is_some());
    assert!(
        IRQ_STATE
            .lock()
            .descs
            .get(&virq)
            .is_some_and(|stored| Arc::ptr_eq(&stored, &line))
    );
    assert!(!line.lock().is_teardown_in_progress());
    assert!(!has_unlocked_drop.load(Ordering::Acquire));
    drop(removed);
    assert!(has_unlocked_drop.load(Ordering::Acquire));
}

#[def_test(serial)]
fn masked_registration_requires_explicit_startup_entry() {
    let virq = 0x5f55;
    let cleanup = ScopedTestLine::for_vacant_slot(virq);
    assert!(cleanup.is_some());
    let (normal, was_absent, startup, has_preserved_mask) = {
        let _pin = kspin::IrqSave::new();
        let normal = crate::try_register_disabled(virq, Arc::new(|_| IrqEvent::HANDLED));
        let was_absent = crate::descriptor(virq).is_none();
        assert_eq!(crate::try_map(IrqDesc::from_virq(virq)), Ok(virq));
        let startup = crate::try_register_startup(
            virq,
            Arc::new(|_| IrqEvent::HANDLED),
            crate::IrqRegisterOptions::disabled(),
        );
        (normal, was_absent, startup, !karch::local_irq_enabled())
    };
    let removed = crate::try_free_irq(virq).unwrap();

    assert!(matches!(normal, Err(IrqDescError::InvalidContext { .. })));
    assert!(was_absent);
    assert_eq!(startup, Ok(true));
    assert!(has_preserved_mask);
    assert!(removed.is_some());
}

#[def_test(serial)]
fn startup_registration_rejects_missing_descriptors_before_preflight() {
    let virq = crate::DYNAMIC_VIRQ_BASE + 0x5fab;
    let cleanup = ScopedTestLine::for_vacant_slot(virq);
    assert!(cleanup.is_some());
    let desc = IrqDesc::from_hwirq(0x5fab)
        .with_virq(virq)
        .with_domain(crate::domain::TEST_DOMAIN);
    let handler: Handler = Arc::new(|_| IrqEvent::HANDLED);
    for spec in [IrqDesc::from_virq(virq), desc] {
        for is_masked in [false, true] {
            let irq_before = karch::local_irq_enabled();
            let guard = is_masked.then(kspin::IrqSave::new);
            let expected_irq = karch::local_irq_enabled();
            let mut has_checked = false;
            let mut has_applied = false;
            let result = super::with_registration_line_checked(
                spec.into(),
                super::ManagementMode::Startup,
                |_| {
                    has_checked = true;
                    Ok(true)
                },
                |_, _, _| {
                    has_applied = true;
                    Ok(())
                },
            );
            assert_eq!(result, Err(IrqDescError::UnknownIrq));
            assert!(!has_checked && !has_applied);
            assert_eq!(
                crate::try_register_startup(
                    spec,
                    handler.clone(),
                    crate::IrqRegisterOptions::disabled(),
                ),
                Err(IrqDescError::UnknownIrq)
            );
            assert_eq!(Arc::strong_count(&handler), 1);
            assert_eq!(karch::local_irq_enabled(), expected_irq);
            drop(guard);
            assert_eq!(karch::local_irq_enabled(), irq_before);
            assert!(crate::descriptor(virq).is_none());
            assert!(
                IRQ_STATE
                    .lock()
                    .translated_hwirq(crate::domain::TEST_DOMAIN, desc.hwirq)
                    .is_none()
            );
        }
    }
}

#[def_test(serial)]
fn task_registration_rejects_missing_descriptors_before_preflight() {
    let virq = crate::DYNAMIC_VIRQ_BASE + 0x5fad;
    let cleanup = ScopedTestLine::for_vacant_slot(virq);
    assert!(cleanup.is_some());
    let desc = IrqDesc::from_hwirq(0x5fad)
        .with_virq(virq)
        .with_domain(crate::domain::TEST_DOMAIN);
    let handler: Handler = Arc::new(|_| IrqEvent::HANDLED);
    for spec in [IrqDesc::from_virq(virq), desc] {
        let mut has_checked = false;
        let mut has_applied = false;
        let result = super::with_registration_line_checked(
            spec.into(),
            super::ManagementMode::Task,
            |_| {
                has_checked = true;
                Ok(true)
            },
            |_, _, _| {
                has_applied = true;
                Ok(())
            },
        );
        assert_eq!(result, Err(IrqDescError::UnknownIrq));
        assert!(!has_checked && !has_applied);
        assert_eq!(
            crate::try_register_disabled(spec, handler.clone()),
            Err(IrqDescError::UnknownIrq)
        );
        assert!(matches!(
            crate::try_register_shared(spec, handler.clone()),
            Err(IrqDescError::UnknownIrq)
        ));
        assert!(crate::descriptor(virq).is_none());
        assert!(
            IRQ_STATE
                .lock()
                .translated_hwirq(crate::domain::TEST_DOMAIN, desc.hwirq)
                .is_none()
        );
        assert_eq!(Arc::strong_count(&handler), 1);
    }
}

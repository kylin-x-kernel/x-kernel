// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use unittest::{assert, assert_eq, assert_ne, def_test};

use super::{IRQ_STATE, IrqLine, IrqStateDesc};
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

// These Lines are software-only fixtures. Rejected richer descriptors must
// never reach platform configuration or publish their offered domain mapping.
enum RegistrationRejection {
    DuplicateRegular,
    IncompatibleShared,
    FullShared,
}

#[expect(
    clippy::unwrap_used,
    reason = "CG-021-no-panic-on-reachable-failure: test scenario: valid fixture requests must \
              return policy rejection and retain the original registered Line"
)]
fn check_registration_rejection(virq: usize, rejection: RegistrationRejection) -> unittest::TestResult {
    let hwirq = crate::DYNAMIC_VIRQ_BASE + virq;
    let desc = IrqDesc::from_hwirq(hwirq).with_virq(virq);
    let richer = desc
        .with_domain(crate::GIC_ROOT_DOMAIN)
        .with_controller(crate::IrqController::Gic)
        .with_source(crate::IrqSource::Acpi);
    let installed: Handler = Arc::new(|_| IrqEvent::HANDLED);
    let offered: Handler = Arc::new(|_| IrqEvent::HANDLED);
    let mut entry = IrqStateDesc::new(desc);
    if matches!(rejection, RegistrationRejection::FullShared) {
        for _ in 0..crate::state::MAX_IRQ_ACTIONS {
            assert!(entry.install_shared_action(installed.clone()).is_some());
        }
    } else {
        assert!(entry.install_regular_action(crate::action::IrqAction::regular(
            installed.clone(),
        )));
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
    assert!(IRQ_STATE.lock().translated_hwirq(crate::GIC_ROOT_DOMAIN, hwirq).is_none());
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
            state.translated_hwirq(crate::GIC_ROOT_DOMAIN, hwirq).is_none(),
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
fn duplicate_regular_registration_preserves_richer_legacy_metadata_and_owners() -> unittest::TestResult {
    check_registration_rejection(0x5f70, RegistrationRejection::DuplicateRegular)
}

#[def_test(serial)]
fn incompatible_shared_registration_preserves_richer_legacy_metadata_and_owners() -> unittest::TestResult {
    check_registration_rejection(0x5f71, RegistrationRejection::IncompatibleShared)
}

#[def_test(serial)]
fn full_shared_registration_preserves_richer_legacy_metadata_and_owners() -> unittest::TestResult {
    check_registration_rejection(0x5f72, RegistrationRejection::FullShared)
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
    assert!(super::try_register_disabled(regular, handler.clone()).unwrap());
    {
        let state = IRQ_STATE.lock();
        let entry = state.descs.get(&regular).unwrap().lock();
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
        let entry = state.descs.get(&shared).unwrap().lock();
        assert_eq!(entry.action_count(), 2);
        assert!(entry.is_enabled_for_tests());
        assert_eq!(entry.disable_depth_for_tests(), 0);
    }
    assert!(super::free_irq_action(shared, first).is_some());
    assert!(super::free_irq_action(shared, second).is_some());
    assert_eq!(Arc::strong_count(&handler), 1);
    let state = IRQ_STATE.lock();
    assert!(!state.descs.contains_key(&regular));
    assert!(!state.descs.contains_key(&shared));
}

kspin::static_lock! {
    static REPLACE_BEFORE_ADMISSION: kspin::SpinNoIrq<Option<ReplaceBeforeAdmission>> =
        kspin::SpinNoIrq::new(None);
}

kspin::static_lock! {
    static CANDIDATE_RETRY: kspin::SpinNoIrq<Option<CandidateRetry>> =
        kspin::SpinNoIrq::new(None);
}

struct CandidateRetry {
    virq: usize,
    competitor: Arc<IrqLine>,
    candidate: Option<Arc<IrqLine>>,
    acquisitions: usize,
}

fn force_candidate_retry(line: &Arc<IrqLine>) {
    let virq = line.lock().desc.logical_irq();
    let hook = {
        let mut pending = CANDIDATE_RETRY.lock();
        if pending.as_ref().is_some_and(|hook| Some(hook.virq) == virq) {
            pending.take()
        } else {
            None
        }
    };
    if let Some(mut hook) = hook {
        let removed = match hook.acquisitions {
            0 => {
                hook.candidate = Some(line.clone());
                IRQ_STATE.lock().descs.insert(hook.virq, hook.competitor.clone())
            }
            1 => IRQ_STATE.lock().descs.remove(&hook.virq),
            _ => None,
        };
        drop(removed);
        hook.acquisitions += 1;
        *CANDIDATE_RETRY.lock() = Some(hook);
    }
}

/// Retires the selected identity before acquiring the real mutex, once only.
pub(super) fn replace_before_management_acquire(line: &Arc<IrqLine>) {
    force_candidate_retry(line);
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
fn registration_reuses_unpublished_candidate_after_directory_churn() {
    let virq = 0x5f56;
    *CANDIDATE_RETRY.lock() = Some(CandidateRetry {
        virq,
        competitor: Arc::new(IrqLine::new(IrqStateDesc::new(IrqDesc::from_virq(virq)))),
        candidate: None,
        acquisitions: 0,
    });
    let registered = crate::try_register_disabled(virq, Arc::new(|_| IrqEvent::HANDLED));
    let hook = CANDIDATE_RETRY.lock().take().unwrap();
    let published = IRQ_STATE.lock().descs.get(&virq).cloned();
    let removed = crate::try_free_irq(virq).unwrap();

    assert_eq!(registered, Ok(true));
    assert_eq!(hook.acquisitions, 3);
    assert!(Arc::ptr_eq(&published.unwrap(), &hook.candidate.unwrap()));
    assert!(removed.is_some());
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
                // Main's prepare methods mutate software state immediately.
                let _plan = entry.prepare_legacy_enable();
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

#[def_test(serial)]
fn rejected_handler_destructor_runs_after_management_unlock() {
    let virq = 0x5f54;
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
    assert!(line.lock().install_regular_action(crate::action::IrqAction::regular(handler)));
    assert!(IRQ_STATE.lock().descs.insert(virq, line.clone()).is_none());
    let removed = crate::try_free_irq(virq).unwrap();
    assert!(removed.is_some());
    assert!(crate::descriptor(virq).is_none());
    assert!(!line.lock().is_teardown_in_progress());
    assert!(!has_unlocked_drop.load(Ordering::Acquire));
    drop(removed);
    assert!(has_unlocked_drop.load(Ordering::Acquire));
}

#[def_test(serial)]
fn masked_registration_requires_explicit_startup_entry() {
    let virq = 0x5f55;
    let (normal, was_absent, startup, has_preserved_mask) = {
        let _pin = kspin::IrqSave::new();
        let normal = crate::try_register_disabled(virq, Arc::new(|_| IrqEvent::HANDLED));
        let was_absent = crate::descriptor(virq).is_none();
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

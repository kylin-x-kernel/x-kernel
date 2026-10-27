// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use unittest::{assert, assert_eq, def_test};

use super::{IRQ_STATE, IrqLine, IrqStateDesc};
use crate::{Handler, IrqDesc, IrqDescError, IrqEvent};

struct ReplaceBeforeAdmission {
    virq: usize,
    original: Arc<IrqLine>,
    replacement: Arc<IrqLine>,
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
    let replacement_desc = IrqDesc::from_hwirq(0x5f53).with_virq(virq);
    let replacement = Arc::new(IrqLine::new(IrqStateDesc::new(replacement_desc)));
    let (original, _replacement_hook) = publish_replaced_on_acquisition(virq, &replacement);
    let registered =
        crate::try_register_disabled(IrqDesc::from_virq(virq), Arc::new(|_| IrqEvent::HANDLED));
    let desc = replacement.lock().desc;
    let count = replacement.lock().action_count();
    let removed = IRQ_STATE.lock().descs.remove(&virq);
    drop(removed);

    assert!(matches!(
        registered,
        Err(IrqDescError::HwirqConflict { .. })
    ));
    assert_eq!(desc, replacement_desc);
    assert_eq!(count, 0);
    assert_eq!(original.lock().action_count(), 0);
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

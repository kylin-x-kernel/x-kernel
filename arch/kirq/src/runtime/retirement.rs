// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-Line retirement notification.
//!
//! Slots are allocated before the Line lock and linked with intrusive storage.
//! Retirement moves the whole list under the Line lock and invokes observers
//! only after every KIRQ lock has been released.

use alloc::sync::{Arc, Weak};

#[cfg(any(target_arch = "x86_64", unittest))]
use intrusive_collections::LinkedList;
use intrusive_collections::{LinkedListAtomicLink, intrusive_adapter};

use super::state::{IRQ_STATE, IrqLine};
use crate::Virq;

/// Receives one exact Line retirement.
pub trait IrqLineRetirementObserver: Send + Sync {
    /// Called after the Line is retired and after KIRQ locks are released.
    ///
    /// Runs in the retirement caller's task context (currently sleepable
    /// `free_msix`). It must not hold or re-enter KIRQ policy, registration or
    /// management locks, and must not wait for an IRQ handler. It may take
    /// filesystem/namespace locks. Cancellation does not wait for an already
    /// captured callback, so implementations must be idempotent.
    fn on_line_retired(&self, virq: Virq);
}

pub(super) struct RetirementSlot {
    /// Only the x86 MSI retirement path notifies observers.
    #[cfg(any(target_arch = "x86_64", unittest))]
    pub(super) observer: Weak<dyn IrqLineRetirementObserver>,
    pub(super) link: LinkedListAtomicLink,
}

intrusive_adapter!(
    pub(super) RetirementAdapter = Arc<RetirementSlot>: RetirementSlot {
        link => LinkedListAtomicLink
    }
);

/// Cancellable registration for one exact Line retirement.
///
/// Drop only unlinks while the Line is live and acquires that Line's short
/// lock. Once retirement has moved the list, the retirement queue owns the
/// slot and Drop does not touch links or wait for a captured callback.
pub struct IrqLineRetirementSubscription {
    pub(super) line: Weak<IrqLine>,
    pub(super) slot: Arc<RetirementSlot>,
}

impl Drop for IrqLineRetirementSubscription {
    fn drop(&mut self) {
        let Some(line) = self.line.upgrade() else {
            return;
        };
        let mut entry = line.lock();
        if !entry.is_retired() {
            entry.remove_retirement(&self.slot);
        }
    }
}

pub(super) fn subscribe(
    virq: Virq,
    line: &Arc<IrqLine>,
    observer: Weak<dyn IrqLineRetirementObserver>,
) -> Result<IrqLineRetirementSubscription, crate::IrqDescError> {
    let slot = Arc::new(RetirementSlot {
        #[cfg(any(target_arch = "x86_64", unittest))]
        observer,
        link: LinkedListAtomicLink::new(),
    });
    #[cfg(not(any(target_arch = "x86_64", unittest)))]
    let _ = observer;
    {
        let state = IRQ_STATE.lock();
        if !state
            .descs
            .get(&virq)
            .is_some_and(|current| Arc::ptr_eq(&current, line))
        {
            return Err(crate::IrqDescError::UnknownIrq);
        }
        let mut entry = line.lock();
        if entry.is_retired() || !entry.register_retirement(slot.clone()) {
            return Err(crate::IrqDescError::UnknownIrq);
        }
    }
    Ok(IrqLineRetirementSubscription {
        line: Arc::downgrade(line),
        slot,
    })
}

#[cfg(any(target_arch = "x86_64", unittest))]
pub(super) fn notify_retirements(virq: Virq, mut slots: LinkedList<RetirementAdapter>) {
    while let Some(slot) = slots.pop_front() {
        if let Some(observer) = slot.observer.upgrade() {
            observer.on_line_retired(virq);
        }
    }
}

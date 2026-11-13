// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Complete scheduler publication accounting and reversible activation admission.

use alloc::{sync::Arc, vec::Vec};
use core::marker::PhantomData;

use intrusive_collections::{LinkedList, LinkedListAtomicLink, intrusive_adapter};
use kspin::{SpinNoIrq, static_lock};
use ktime_types::TimeSpan;

use super::{FreezeCancellation, FreezeError, FrozenTasks, freeze_tasks};
use crate::{KtaskRef, TaskInner, WeakKtaskRef};

enum Publication {
    Prepared,
    Active,
    Deferred(KtaskRef),
    Retired,
}

/// Preallocated before scheduler publication; links belong only to REGISTRY.
pub(crate) struct Registration {
    links: LinkedListAtomicLink,
    task: WeakKtaskRef,
    publication: SpinNoIrq<Publication>,
}

impl Registration {
    pub(crate) fn new(task: &KtaskRef) -> Self {
        Self {
            links: LinkedListAtomicLink::new(),
            task: KtaskRef::downgrade(task),
            publication: SpinNoIrq::new(Publication::Prepared),
        }
    }
}

intrusive_adapter!(RegistrationAdapter = Arc<Registration>: Registration {
    links => LinkedListAtomicLink
});

struct Registry {
    active: LinkedList<RegistrationAdapter>,
    deferred: LinkedList<RegistrationAdapter>,
    is_closed: bool,
}

impl Registry {
    const fn new() -> Self {
        Self {
            active: LinkedList::new(RegistrationAdapter::NEW),
            deferred: LinkedList::new(RegistrationAdapter::NEW),
            is_closed: false,
        }
    }

    fn admit(&mut self, registration: &Arc<Registration>, task: &KtaskRef) -> bool {
        let mut publication = registration.publication.lock();
        // Only Prepared registrations are unlinked; relinking would panic.
        if !matches!(*publication, Publication::Prepared) {
            return false;
        }
        if self.is_closed {
            *publication = Publication::Deferred(task.clone());
            self.deferred.push_back(registration.clone());
            false
        } else {
            *publication = Publication::Active;
            self.active.push_back(registration.clone());
            true
        }
    }

    fn retire(&mut self, registration: &Arc<Registration>) -> Option<Arc<Registration>> {
        let mut publication = registration.publication.lock();
        let removed = if matches!(*publication, Publication::Active) {
            debug_assert!(registration.links.is_linked());
            // SAFETY: this node is linked only by admit, to this registry's
            // active list while Active; Active is left only here, after removal.
            // The registry lock covers state and links, so it is a member now.
            unsafe { self.active.cursor_mut_from_ptr(Arc::as_ptr(registration)) }.remove()
        } else {
            None
        };
        // A task cannot exit before its deferred activation is released.
        if !matches!(*publication, Publication::Deferred(_)) {
            *publication = Publication::Retired;
        }
        removed
    }
}

static_lock! {
    static REGISTRY: SpinNoIrq<Registry> = SpinNoIrq::new(Registry::new());
}

/// Called only for first activation, not for wakeup/migration of admitted tasks.
pub(crate) fn admit(task: &KtaskRef) -> bool {
    let Some(registration) = task.suspend_registration.get() else {
        // Idle tasks are installed by scheduler bring-up, never activated here.
        return false;
    };
    REGISTRY.lock().admit(registration, task)
}

pub(crate) fn retire(task: &TaskInner) {
    if let Some(registration) = task.suspend_registration.get() {
        let removed = REGISTRY.lock().retire(registration);
        drop(removed);
    }
}

/// Keeps scheduler admission closed while taking a complete active-task snapshot.
pub(super) struct PublicationGate(PhantomData<*mut ()>);

impl PublicationGate {
    pub(super) fn acquire() -> Result<Self, FreezeError> {
        let mut registry = REGISTRY.lock();
        if registry.is_closed {
            return Err(FreezeError::Busy);
        }
        registry.is_closed = true;
        Ok(Self(PhantomData))
    }

    pub(super) fn snapshot(&self) -> Vec<KtaskRef> {
        // Closing admission prevents growth; retirements may only shrink it.
        // Allocate outside the IRQ-safe list lock, then fill without growing.
        let capacity = REGISTRY.lock().active.iter().count();
        let mut tasks = Vec::with_capacity(capacity);
        {
            let registry = REGISTRY.lock();
            for registration in registry.active.iter() {
                if let Some(task) = registration.task.upgrade() {
                    tasks.push(task);
                }
            }
        }
        tasks
    }
}

impl Drop for PublicationGate {
    fn drop(&mut self) {
        let mut deferred = {
            let mut registry = REGISTRY.lock();
            registry.is_closed = false;
            registry.deferred.take()
        };
        while let Some(registration) = deferred.pop_front() {
            let publication = {
                let mut state = registration.publication.lock();
                core::mem::replace(&mut *state, Publication::Prepared)
            };
            if let Publication::Deferred(task) = publication {
                // Re-enter admission: a subsequent PM attempt may already have
                // closed it again. No scheduler lock is taken under REGISTRY.
                crate::activate_task(&task);
            }
        }
    }
}

/// Owns a complete frozen task set and deferred scheduler publication.
/// Dropping thaws existing tasks before releasing deferred activations.
/// Retain on the coordinating task and drop only after recovery, outside PM
/// dependency locks, with ordinary task execution safe to resume.
#[must_use = "retain until PM cleanup has made ordinary execution safe"]
pub struct SystemFrozenTasks {
    // Fields drop in declaration order: thaw existing tasks before reopening
    // admission and releasing deferred activations. Keep this field first.
    _frozen: FrozenTasks,
    _gate: PublicationGate,
}

/// Freezes every admitted non-idle task except the coordinator and listed kernel infrastructure.
///
/// Closing admission and registering a first activation use the same lock.
/// Tasks prepared or activated afterwards cannot execute until the returned
/// guard is dropped. Detached construction and process-domain visibility may
/// still occur; scheduler activation is deferred without blocking its caller.
/// The snapshot is independent of the bounded, optional diagnostic registry.
///
/// `exempt` explicitly lists kernel infrastructure needed for PM progress.
/// User tasks, idle tasks, the coordinator, duplicates and unregistered tasks
/// are rejected. No worker is exempted by name or merely because it is blocked.
/// The platform must quiesce exempt consumers before their resources disappear.
/// Missing checkpoints time out and reopen admission after thawing partial
/// acknowledgments. This API does not stop CPUs, IRQs, devices or workqueues.
///
/// # Errors
/// Returns context, exemption, contention or cooperative-freezer errors.
/// Every ordinary failure releases admission and all installed freeze requests.
pub fn freeze_system_tasks(
    exempt: &[KtaskRef],
    timeout: TimeSpan,
    cancellation: &FreezeCancellation,
) -> Result<SystemFrozenTasks, FreezeError> {
    if !crate::is_sleepable_context() {
        return Err(FreezeError::InvalidContext);
    }
    let gate = PublicationGate::acquire()?;
    freeze_with_gate(gate, exempt, timeout, cancellation)
}

// Consume an already-closed gate so exemption selection in private scheduler
// fixtures can share the same admission interval as the production freeze.
fn freeze_with_gate(
    gate: PublicationGate,
    exempt: &[KtaskRef],
    timeout: TimeSpan,
    cancellation: &FreezeCancellation,
) -> Result<SystemFrozenTasks, FreezeError> {
    let current = crate::current().clone();
    let mut tasks = gate.snapshot();
    for (index, task) in exempt.iter().enumerate() {
        if task.user_runtime().is_some()
            || task.is_idle()
            || KtaskRef::ptr_eq(task, &current)
            || exempt[..index]
                .iter()
                .any(|other| KtaskRef::ptr_eq(other, task))
            || (!task
                .is_exit_complete
                .load(core::sync::atomic::Ordering::Acquire)
                && !tasks.iter().any(|active| KtaskRef::ptr_eq(active, task)))
        {
            return Err(FreezeError::InvalidTaskSet);
        }
    }
    tasks.retain(|task| {
        !KtaskRef::ptr_eq(task, &current)
            && !task
                .is_exit_complete
                .load(core::sync::atomic::Ordering::Acquire)
            && !exempt.iter().any(|other| KtaskRef::ptr_eq(other, task))
    });
    let frozen = freeze_tasks(&tasks, timeout, cancellation)?;
    Ok(SystemFrozenTasks {
        _frozen: frozen,
        _gate: gate,
    })
}

#[cfg(unittest)]
unittest::path_mod_test!(tests, "../../ut-tests/freezer/system.rs");

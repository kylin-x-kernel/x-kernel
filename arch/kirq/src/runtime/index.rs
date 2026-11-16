// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Owning IRQ indexes. Registry exclusion serializes writers, RCU protects readers.
//!
//! This module owns XArray preparation and deferred storage retirement, not
//! descriptor compatibility policy. Registry commits coordinate both indexes
//! under exclusion; dispatch uses their retained or borrowed Line identities.
//! An index lifetime never substitutes for action execution synchronization.

use alloc::sync::Arc;

use kalloc_types::AllocFlags;
#[cfg(any(target_arch = "x86_64", unittest))]
use kxarray::{PendingRetirement, PreparedRemove};
use kxarray::{PreparedInsert, XArray};

use super::state::IrqLine;
use crate::{IrqDescError, IrqDomainId, IrqRef, Virq};

pub(crate) static DIRECTORY: XArray<Arc<IrqLine>> = XArray::new();

/// Borrows the exact indexed Line without refcount updates or writer locks.
/// Keep the scope through handler use and original controller completion.
pub(super) fn borrowed<'scope>(
    source: IrqRef,
    scope: &'scope krcu::ReadScope<'_>,
) -> Option<&'scope IrqLine> {
    match source {
        IrqRef::Virq(virq) => DIRECTORY.read(scope, virq),
        IrqRef::Domain(id, hwirq) => crate::domain::domain(id)?.lines.read(scope, hwirq),
    }
}

/// Retain before leaving the scope; the Line lock separately grants admission.
pub(crate) fn owned(array: &XArray<Arc<IrqLine>>, key: usize) -> Option<Arc<IrqLine>> {
    if kiface_irq_context_api::InterruptContextIf::is_in_interrupt_handler() {
        krcu::with_interrupt_read(|scope| array.get_owned(scope, key))
    } else {
        // Boot/control callers need no initialized current task or RCU service.
        array.get_owned_locked(key)
    }
}

pub(crate) fn lookup(source: IrqRef) -> Option<(Virq, Option<Arc<IrqLine>>)> {
    let virq = match source {
        IrqRef::Virq(virq) => virq,
        IrqRef::Domain(id, hwirq) => {
            let domain = crate::domain::domain(id)?;
            if let Some(line) = owned(&domain.lines, hwirq) {
                return Some((line.logical_irq()?, Some(line)));
            }
            domain.resolve_unmapped(hwirq)?
        }
    };
    Some((virq, owned(&DIRECTORY, virq)))
}

enum Storage {
    /// Borrowed, stateless view of an index owned elsewhere.
    ///
    /// A borrowed array has no owned population, so no length is tracked here:
    /// callers may construct this view for one transaction and drop it without
    /// pretending to know how many entries the owner already holds.
    Shared(&'static XArray<Arc<IrqLine>>),
    #[cfg(unittest)]
    /// Private fixture index whose entry count belongs to this object.
    Private {
        array: XArray<Arc<IrqLine>>,
        count: usize,
    },
}

/// A control-plane view of an index, never a second copy of its entries.
///
/// Only private fixture storage owns a count. The global directory and every
/// domain index are borrowed views; their entries outlive any temporary view.
pub(super) struct LineIndex {
    storage: Storage,
}

impl LineIndex {
    #[cfg(unittest)]
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: synthetic fixture allocation failure \
                  aborts the test"
    )]
    pub(super) fn insert(&mut self, key: usize, line: Arc<IrqLine>) -> Option<Arc<IrqLine>> {
        let old = self.remove(&key);
        // SAFETY: test callers publish synthetic sources from allocation-capable
        // task context, not hardware handlers; NOWAIT forbids sleeping reclaim.
        let prepared = unsafe { self.array().prepare_insert(key, AllocFlags::NOWAIT) }
            .expect("fixture index preparation");
        self.publish(line, prepared);
        old
    }

    #[cfg(unittest)]
    pub(super) fn remove(&mut self, key: &usize) -> Option<Arc<IrqLine>> {
        // SAFETY: fixture callers own synthetic keys and join lookup users
        // before teardown. Identity-swap tests run after lookup returned an
        // owned Arc, not during an RCU borrow. No real device produces this key.
        // Nodes are not pruned, so unrelated hardware readers are unaffected.
        let old = unsafe { self.array().take_quiescent_fixture(*key) };
        if old.is_some() {
            self.note_withdrawn();
        }
        old
    }

    pub(super) const fn shared(array: &'static XArray<Arc<IrqLine>>) -> Self {
        Self {
            storage: Storage::Shared(array),
        }
    }

    #[cfg(unittest)]
    pub(super) const fn new() -> Self {
        Self {
            storage: Storage::Private {
                array: XArray::new(),
                count: 0,
            },
        }
    }

    pub(super) fn array(&self) -> &XArray<Arc<IrqLine>> {
        match &self.storage {
            Storage::Shared(array) => array,
            #[cfg(unittest)]
            Storage::Private { array, .. } => array,
        }
    }

    /// Entry count for a private fixture index, or `None` for a borrowed view.
    #[cfg(unittest)]
    fn private_count(&self) -> Option<&usize> {
        match &self.storage {
            Storage::Shared(_) => None,
            Storage::Private { count, .. } => Some(count),
        }
    }

    /// Mutable entry count for a private fixture index, or `None` for a view.
    #[cfg(unittest)]
    fn private_count_mut(&mut self) -> Option<&mut usize> {
        match &mut self.storage {
            Storage::Shared(_) => None,
            Storage::Private { count, .. } => Some(count),
        }
    }

    pub(super) fn get(&self, key: &usize) -> Option<Arc<IrqLine>> {
        owned(self.array(), *key)
    }

    pub(super) fn contains_key(&self, key: &usize) -> bool {
        self.get(key).is_some()
    }

    /// Number of entries owned by a private fixture index.
    ///
    /// Borrowed views deliberately have no length: entries belong to the array
    /// owner, not to this temporary view.
    #[cfg(unittest)]
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: only private fixture indexes own a \
                  population; callers that need a length operate on fixture state"
    )]
    pub(super) fn len(&self) -> usize {
        *self
            .private_count()
            .expect("borrowed IRQ index does not own a length")
    }

    #[cfg(unittest)]
    pub(super) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: registry exclusion spans vacancy \
                  validation and prepared insertion"
    )]
    pub(super) fn publish(&mut self, line: Arc<IrqLine>, prepared: PreparedInsert) {
        // SAFETY: registry writers prepared capacity before exclusion and
        // certified the Line release chain; IRQ/NMI readers never write.
        unsafe { self.array().insert_prepared(line, prepared) }
            .map_err(|(error, _)| error)
            .expect("prevalidated IRQ index vacancy");
        self.note_published();
    }

    /// Records an insertion only for an index that owns its population.
    fn note_published(&mut self) {
        #[cfg(unittest)]
        if let Some(count) = self.private_count_mut() {
            *count += 1;
        }
    }

    /// Records a withdrawal only for an index that owns its population.
    #[cfg(any(target_arch = "x86_64", unittest))]
    fn note_withdrawn(&mut self) {
        #[cfg(unittest)]
        if let Some(count) = self.private_count_mut() {
            *count -= 1;
        }
    }

    /// # Safety
    /// Preflight before registry exclusion; submit every returned record after
    /// releasing exclusion and restoring that same task context.
    #[cfg(any(target_arch = "x86_64", unittest))]
    pub(super) unsafe fn withdraw(
        &mut self,
        key: usize,
        prepared: PreparedRemove<Arc<IrqLine>>,
    ) -> Option<PendingRetirement<Arc<IrqLine>>> {
        // SAFETY: caller preflight precedes registry exclusion and all returned
        // retirements are submitted only after restoring that context.
        let pending = unsafe { self.array().withdraw_prepared(key, prepared) };
        if pending.is_some() {
            self.note_withdrawn();
        }
        pending
    }
}

/// Private allocation budget for a registry commit across two indexes.
/// Construction changes no published state; the registry consumes each prepared
/// insertion only after validating the complete descriptor transaction.
pub(super) struct PreparedPublication {
    pub(super) logical: Option<PreparedInsert>,
    pub(super) domain: Option<PreparedInsert>,
}

impl PreparedPublication {
    pub(super) fn new(
        virq: Virq,
        mapping: Option<(IrqDomainId, usize)>,
        needs_logical: bool,
    ) -> Result<Self, IrqDescError> {
        if (needs_logical || mapping.is_some())
            && (kiface_irq_context_api::InterruptContextIf::is_in_interrupt_context()
                || krcu::is_reading())
        {
            return Err(IrqDescError::InvalidContext {
                operation: "publish IRQ index",
            });
        }
        // Preparation touches no live tree and NOWAIT permits early boot and
        // IRQ-masked task setup. These APIs still exclude IRQ/NMI/BH callers.
        let prepare = |key| {
            #[cfg(unittest)]
            tests_preparation::check_preparation(virq, key)?;
            // SAFETY: callers are boot/control tasks with initialized allocator
            // and local primitives. NOWAIT forbids sleeping reclaim.
            unsafe { DIRECTORY.prepare_insert(key, AllocFlags::NOWAIT) }
                .map_err(|_| IrqDescError::NoMemory)
        };
        Ok(Self {
            logical: needs_logical.then(|| prepare(virq)).transpose()?,
            domain: mapping.map(|(_, hwirq)| prepare(hwirq)).transpose()?,
        })
    }
}

unittest::path_mod_test!(tests_preparation, "../../ut-tests/index_preparation.rs");

#[cfg(any(target_arch = "x86_64", unittest))]
/// Allocation-time resources for eventual MSI removal from both indexes.
/// Stored with the Line so a valid release does not introduce a new OOM path.
pub(super) struct PreparedWithdrawal {
    pub(super) logical: PreparedRemove<Arc<IrqLine>>,
    pub(super) domain: PreparedRemove<Arc<IrqLine>>,
}

#[cfg(any(target_arch = "x86_64", unittest))]
impl PreparedWithdrawal {
    pub(super) fn new(hwirq: usize) -> Option<Self> {
        #[cfg(not(unittest))]
        let _ = hwirq;
        #[cfg(unittest)]
        tests_preparation::check_retirement_preparation(hwirq, 0)?;
        // SAFETY: MSI publication runs in allocator-capable boot/task context.
        // NOWAIT prepares private storage only; no RCU service is required yet.
        let logical = unsafe { DIRECTORY.prepare_remove(AllocFlags::NOWAIT) }.ok()?;
        #[cfg(unittest)]
        tests_preparation::check_retirement_preparation(hwirq, 1)?;
        // SAFETY: the same caller prepares the second private record before
        // registry exclusion. Failure drops only the unused first record.
        let domain = unsafe { DIRECTORY.prepare_remove(AllocFlags::NOWAIT) }.ok()?;
        Some(Self { logical, domain })
    }
}

#[cfg(any(target_arch = "x86_64", unittest))]
/// Detached index owners awaiting RCU retirement outside registry exclusion.
/// This is storage reclamation, not device shutdown or an action drain.
pub(super) struct WithdrawnIndexes {
    pub(super) logical: Option<PendingRetirement<Arc<IrqLine>>>,
    pub(super) domain: Option<PendingRetirement<Arc<IrqLine>>>,
}

#[cfg(any(target_arch = "x86_64", unittest))]
impl WithdrawnIndexes {
    pub(super) fn retire(self) {
        for pending in [self.domain, self.logical].into_iter().flatten() {
            // SAFETY: only the MSI transaction constructs this result, after
            // preflight and withdrawal; its caller restores exclusion before
            // submission, without arbitrary callbacks or context transitions.
            unsafe { pending.retire() };
        }
    }
}

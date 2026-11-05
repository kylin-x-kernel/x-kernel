// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Owning IRQ indexes. Registry exclusion serializes writers, RCU protects readers.

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
    Shared(&'static XArray<Arc<IrqLine>>),
    #[cfg(unittest)]
    Private(XArray<Arc<IrqLine>>),
}

/// A control-plane view of an index, never a second copy of its entries.
pub(super) struct LineIndex {
    storage: Storage,
    count: usize,
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
            self.count -= 1;
        }
        old
    }

    pub(super) const fn shared(array: &'static XArray<Arc<IrqLine>>) -> Self {
        Self {
            storage: Storage::Shared(array),
            count: 0,
        }
    }

    #[cfg(unittest)]
    pub(super) const fn new() -> Self {
        Self {
            storage: Storage::Private(XArray::new()),
            count: 0,
        }
    }

    pub(super) fn array(&self) -> &XArray<Arc<IrqLine>> {
        match &self.storage {
            Storage::Shared(array) => array,
            #[cfg(unittest)]
            Storage::Private(array) => array,
        }
    }

    pub(super) fn get(&self, key: &usize) -> Option<Arc<IrqLine>> {
        owned(self.array(), *key)
    }

    pub(super) fn contains_key(&self, key: &usize) -> bool {
        self.get(key).is_some()
    }

    #[cfg(unittest)]
    pub(super) const fn len(&self) -> usize {
        self.count
    }

    #[cfg(unittest)]
    pub(super) const fn is_empty(&self) -> bool {
        self.count == 0
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
        self.count += 1;
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
            self.count -= 1;
        }
        pending
    }
}

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

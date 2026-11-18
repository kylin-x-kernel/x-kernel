// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Global IRQ namespaces and mapping/MSI transactions.
//!
//! Registry exclusion serializes logical-number allocation, original-Line
//! membership checks and paired logical/domain index publication or withdrawal.
//! Lock order is Line management mutex, registry, then Line state; dispatch
//! retains Lines through the RCU indexes without taking the registry lock.
//! Candidates and production index capacity are prepared outside short locks.
//! Withdrawal closes Line admission before unlinking either index and transfers
//! retirement records for submission after exclusion ends.
//!
//! This child module may update private Line identity/lifecycle fields as part
//! of a transaction. It does not own handler execution, source quiescence,
//! action reclamation or controller policy; those remain with their owners.

use alloc::sync::Arc;

use kspin::{SpinNoIrq, static_lock};

use super::{IrqLine, IrqStateDesc};
#[cfg(unittest)]
use crate::IrqFlags;
#[cfg(any(target_arch = "x86_64", unittest))]
use crate::backend::msi::MsiReservation;
#[cfg(any(target_arch = "x86_64", unittest))]
use crate::runtime::index::{PreparedWithdrawal, WithdrawnIndexes};
#[cfg(any(target_arch = "x86_64", unittest))]
use crate::runtime::retirement::{RetirementAdapter, notify_retirements};
use crate::{
    Hwirq, IrqDesc, IrqDescError, IrqDomainId, IrqSpec, Virq,
    runtime::index::{LineIndex, PreparedPublication},
};

static_lock! {
    /// IRQ control-plane aggregate.
    ///
    /// Descriptor membership and `(domain, hwirq) -> virq` namespace changes go
    /// through this lock. Hardware dispatch retains a Line through the RCU index
    /// without this lock, then takes only the Line lock to obtain admission.
    pub(in crate::runtime) static IRQ_STATE: SpinNoIrq<IrqState> = SpinNoIrq::new(IrqState::global());
}
/// First logical number considered by dynamic allocation; not a reserved lease.
pub const DYNAMIC_VIRQ_BASE: Virq = 4096;

/// Result of resolving one caller-provided IRQ descriptor.
///
/// Both prepared index insertions have completed before this result is returned.
pub(in crate::runtime) struct ResolvedIrqDesc {
    desc: IrqDesc,
}

impl ResolvedIrqDesc {
    const fn new(desc: IrqDesc) -> Self {
        Self { desc }
    }

    pub(in crate::runtime) const fn into_desc(self) -> IrqDesc {
        self.desc
    }
}

/// Planned identity, not an installed mapping or a reservation of the Line.
/// Planning does not acquire a Line lock. Descriptor and mapping checks precede
/// mutation in commit_resolution under continuous registry and Line exclusion.
pub(in crate::runtime) struct IrqResolution {
    pub(in crate::runtime) virq: Virq,
    pub(in crate::runtime) offered: Option<IrqDesc>,
}

/// Registry-owned namespace bindings and dynamic identity allocation.
///
/// Global mutations require `IRQ_STATE` exclusion; private test registries have
/// exclusive fixture ownership. Both indexes retain the same original Line.
/// Publication validates identity and flow before inserting either binding;
/// retirement marks that Line retired before withdrawal, so retained readers
/// cannot admit work on it or redirect to a same-number successor.
/// Per-Line action/control state remains behind the Line lock, not this type.
pub(in crate::runtime) struct IrqState {
    /// Primary OS-visible IRQ namespace.
    ///
    /// `virq` is the stable key used by registration, unregister, descriptor
    /// queries, and normal action dispatch.
    pub(in crate::runtime) descs: LineIndex,
    /// Private per-domain control-plane state for test registries.
    ///
    /// The global registry keeps no copy: each domain owns its own index, and
    /// [`Self::with_domain_index`] resolves it through the domain registry so
    /// dynamically allocated per-instance domains need no fixed slot.
    /// Dispatch reads the shared XArray through RCU without registry exclusion.
    #[cfg(unittest)]
    domain_states: [LineIndex; crate::domain::DOMAIN_COUNT],
    /// Whether this registry owns the private test indexes above.
    #[cfg(unittest)]
    private_domains: bool,
    /// Next dynamically allocated OS-visible IRQ number.
    ///
    /// Caller-provided `virq` values occupy the explicit OS-visible namespace;
    /// dynamically assigned descriptors start here so they do not collide with
    /// ordinary low platform line numbers.
    next_virq: Virq,
    #[cfg(unittest)]
    index_observer: Option<tests_index_transactions::IndexObserver>,
}

impl IrqState {
    const fn global() -> Self {
        Self {
            descs: LineIndex::shared(&crate::runtime::index::DIRECTORY),
            // The global registry resolves domain indexes from the domains
            // themselves; these views keep the const initializer uniform but
            // are not consulted while `private_domains` is false.
            #[cfg(unittest)]
            domain_states: [
                LineIndex::shared(&crate::domain::GIC_IRQ_DOMAIN.lines),
                LineIndex::shared(&crate::domain::PLIC_IRQ_DOMAIN.lines),
                LineIndex::shared(&crate::domain::IO_APIC_IRQ_DOMAIN.lines),
                LineIndex::shared(&crate::domain::MSI_IRQ_DOMAIN.lines),
                LineIndex::shared(&crate::domain::TEST_IRQ_DOMAIN.lines),
                LineIndex::shared(&crate::domain::SECOND_TEST_IRQ_DOMAIN.lines),
            ],
            #[cfg(unittest)]
            private_domains: false,
            next_virq: DYNAMIC_VIRQ_BASE,
            #[cfg(unittest)]
            index_observer: None,
        }
    }

    #[cfg(unittest)]
    pub(in crate::runtime) const fn new() -> Self {
        Self {
            descs: LineIndex::new(),
            domain_states: [const { LineIndex::new() }; crate::domain::DOMAIN_COUNT],
            private_domains: true,
            next_virq: DYNAMIC_VIRQ_BASE,
            index_observer: None,
        }
    }

    /// Runs `visit` against the control-plane index for `domain`.
    ///
    /// Global transactions use the domain's own `lines` array, so a
    /// dynamically allocated per-instance domain needs no fixed slot. Private
    /// test registries keep isolated indexes for the well-known domains.
    ///
    /// The global path borrows a stateless view: the array owns its entries and
    /// the population count. A temporary borrowed view must never carry a count,
    /// because publication and withdrawal mutate the array directly and only a
    /// private fixture index keeps its own bookkeeping count.
    fn with_domain_index<R>(
        &self,
        domain: IrqDomainId,
        visit: impl FnOnce(&LineIndex) -> R,
    ) -> Option<R> {
        #[cfg(unittest)]
        if self.private_domains {
            return self
                .domain_states
                .get(crate::domain::slot(domain)?)
                .map(visit);
        }
        Some(visit(&LineIndex::shared(
            &crate::domain::domain(domain)?.lines,
        )))
    }

    fn with_domain_index_mut<R>(
        &mut self,
        domain: IrqDomainId,
        visit: impl FnOnce(&mut LineIndex) -> R,
    ) -> Option<R> {
        #[cfg(unittest)]
        if self.private_domains {
            return self
                .domain_states
                .get_mut(crate::domain::slot(domain)?)
                .map(visit);
        }
        let mut index = LineIndex::shared(&crate::domain::domain(domain)?.lines);
        Some(visit(&mut index))
    }

    pub(in crate::runtime) fn alloc_virq(&mut self) -> Result<Virq, IrqDescError> {
        loop {
            let virq = self.next_virq;
            self.next_virq = self
                .next_virq
                .checked_add(1)
                .ok_or(IrqDescError::VirqExhausted { next: virq })?;
            if !self.descs.contains_key(&virq) {
                return Ok(virq);
            }
        }
    }

    /// Publishes a private allocation as a new identity, never an existing binding.
    #[cfg(unittest)]
    fn insert_msi_reservation(
        &mut self,
        desc: IrqDesc,
        line: &Arc<IrqLine>,
    ) -> Result<ResolvedIrqDesc, IrqDescError> {
        validate_resolution_domain(IrqSpec::Desc(desc))?;
        let virq = self.alloc_virq()?;
        let resolution = IrqResolution {
            virq,
            offered: Some(desc.with_virq(virq)),
        };
        let mut prepared =
            PreparedPublication::new(virq, desc.domain.map(|domain| (domain, desc.hwirq)), true)?;
        self.commit_resolution(resolution, line, &mut line.lock(), &mut prepared)
    }

    /// Resolves under the registry lock using a fresh candidate if absent.
    ///
    /// The caller retains `prepared` until outside short locks. An existing
    /// entry ignores it; a missing entry requires an unpublished candidate.
    #[cfg(unittest)]
    pub(in crate::runtime) fn try_resolve_desc(
        &mut self,
        desc: IrqDesc,
        prepared: Option<&Arc<IrqLine>>,
    ) -> Result<ResolvedIrqDesc, IrqDescError> {
        let resolution = self.plan_resolution(IrqSpec::Desc(desc))?;
        self.resolve_planned(resolution, prepared)
    }

    pub(in crate::runtime) fn plan_resolution(
        &mut self,
        spec: IrqSpec,
    ) -> Result<IrqResolution, IrqDescError> {
        let desc = match spec {
            IrqSpec::PlainVirq(virq) => {
                return Ok(IrqResolution {
                    virq,
                    offered: None,
                });
            }
            IrqSpec::Desc(desc) => desc,
        };
        let virq = if let Some(virq) = desc.logical_irq() {
            virq
        } else if let Some(domain) = desc.domain {
            match self.domain_virq(domain, desc.hwirq) {
                Some(virq) => virq,
                None => self.alloc_virq()?,
            }
        } else {
            desc.hwirq
        };
        Ok(IrqResolution {
            virq,
            offered: Some(desc.with_virq(virq)),
        })
    }

    #[cfg(unittest)]
    fn resolve_planned(
        &mut self,
        resolution: IrqResolution,
        prepared: Option<&Arc<IrqLine>>,
    ) -> Result<ResolvedIrqDesc, IrqDescError> {
        // Map and registration callers prepare absent Lines outside short
        // locks and retain that owner until resolution has returned.
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: callers recheck registry identity and \
                      supply a retained candidate before resolving an absent Line"
        )]
        let line = self
            .descs
            .get(&resolution.virq)
            .or_else(|| prepared.cloned())
            .expect("missing Line requires a prepared candidate")
            .clone();
        let mut entry = line.lock();
        let mapping = resolution
            .offered
            .and_then(|desc| desc.domain.map(|domain| (domain, desc.hwirq)));
        let mut publication = PreparedPublication::new(resolution.virq, mapping, true)?;
        self.commit_resolution(resolution, &line, &mut entry, &mut publication)
    }

    // The supplied entry is locked from this Line. Registry exclusion must
    // remain held from planning through commit, so identity cannot be replaced.
    pub(in crate::runtime) fn commit_resolution(
        &mut self,
        resolution: IrqResolution,
        line: &Arc<IrqLine>,
        entry: &mut IrqStateDesc,
        prepared: &mut PreparedPublication,
    ) -> Result<ResolvedIrqDesc, IrqDescError> {
        // A retained old owner is not an unpublished candidate. Reject before
        // selecting a chip or modifying either mapping index.
        if entry.is_retired {
            return Err(IrqDescError::UnknownIrq);
        }
        let virq = resolution.virq;
        if line.logical_irq().is_some_and(|assigned| assigned != virq) {
            return Err(IrqDescError::UnknownIrq);
        }
        let is_existing = if let Some(current) = self.descs.get(&virq) {
            if !Arc::ptr_eq(&current, line) {
                return Err(IrqDescError::UnknownIrq);
            }
            true
        } else {
            false
        };
        let stored_desc = match (is_existing, resolution.offered) {
            (true, Some(desc)) => entry.desc.try_merge(desc)?,
            (true, None) => entry.desc,
            (false, offered) => offered.unwrap_or_else(|| IrqDesc::from_virq(virq)),
        };
        let flow = entry.select_binding_flow(stored_desc)?;
        // Flow operations and source identity must stay fixed while a handler
        // or queued service can still use this Line, or a failed routing operation
        // still needs its original capability for recovery. Validate before mapping writes.
        if is_existing
            && entry.is_binding_in_use()
            && (entry.flow != Some(flow) || entry.desc != stored_desc)
        {
            return Err(IrqDescError::UnsupportedFlow {
                hwirq: stored_desc.hwirq,
            });
        }
        let mapping_to_insert = if let Some(desc) = resolution.offered
            && let Some(domain) = desc.domain
        {
            let existing_virq = self.domain_virq(domain, desc.hwirq);
            if let Some(existing) = existing_virq
                && existing != virq
            {
                return Err(IrqDescError::MappingConflict {
                    domain,
                    hwirq: desc.hwirq,
                    existing,
                    newer: virq,
                });
            }
            existing_virq
                .is_none()
                .then_some((domain, desc.hwirq, virq))
        } else {
            None
        };
        if let Some((domain, hwirq, virq)) = mapping_to_insert
            && let Some(existing_domain) = is_existing.then_some(entry.desc.domain).flatten()
            && self.domain_virq(existing_domain, entry.desc.hwirq) == Some(virq)
        {
            return Err(IrqDescError::VirqMappingConflict {
                virq,
                existing_domain,
                existing_hwirq: entry.desc.hwirq,
                newer_domain: domain,
                newer_hwirq: hwirq,
            });
        }
        // All recoverable validation precedes identity assignment. Registry and
        // Line exclusion serialize this non-allocating, non-reentrant initializer;
        // readers use only Once::get, never an initialization/wait operation.
        // Both capacity checks precede either publication. A racing RCU reader
        // can retain a partially published Line, but its lock prevents admission
        // until descriptor/flow initialization and both insertions complete.
        if (!is_existing && prepared.logical.is_none())
            || (mapping_to_insert.is_some() && prepared.domain.is_none())
        {
            return Err(IrqDescError::NoMemory);
        }
        line.virq.call_once(|| virq);
        entry.flow = Some(flow);
        if is_existing {
            entry.update_desc(stored_desc);
        } else {
            // First publication preserves the candidate's initial generation.
            entry.desc = stored_desc;
            entry.runtime.remember_desc(stored_desc);
        }
        if let Some((domain, ..)) = mapping_to_insert {
            self.publish_domain_mapping(domain, line, prepared);
        }
        #[cfg(unittest)]
        self.observe_index_transition(line, entry);
        if !is_existing {
            #[expect(
                clippy::expect_used,
                reason = "CG-021-no-panic-on-reachable-failure: logical publication capacity \
                          validated before either insertion"
            )]
            self.descs.publish(
                line.clone(),
                prepared.logical.take().expect("prepared logical insertion"),
            );
        }
        Ok(ResolvedIrqDesc::new(stored_desc))
    }

    pub(in crate::runtime) fn lookup_virq(&self, spec: IrqSpec) -> Option<Virq> {
        match spec {
            IrqSpec::PlainVirq(virq) => Some(virq),
            IrqSpec::Desc(desc) => desc.logical_irq().or_else(|| {
                desc.domain
                    .and_then(|domain| self.domain_virq(domain, desc.hwirq))
            }),
        }
    }

    pub(in crate::runtime) fn mapping_to_prepare(
        &self,
        spec: IrqSpec,
    ) -> Option<(IrqDomainId, Hwirq)> {
        let IrqSpec::Desc(desc) = spec else {
            return None;
        };
        let domain = desc.domain?;
        self.domain_virq(domain, desc.hwirq)
            .is_none()
            .then_some((domain, desc.hwirq))
    }

    /// Resolves a claim without looking up a mapped Line a second time by number.
    ///
    /// The outer option is an unresolved source. A resolved identity source can
    /// have no Line; dispatch still reports that number and completes its claim.
    /// Keep registry exclusion until the returned Line is retained and admitted.
    #[cfg(unittest)]
    pub(in crate::runtime) fn lookup_dispatch_line(
        &self,
        source: crate::IrqRef,
    ) -> Option<(Virq, Option<Arc<IrqLine>>)> {
        let virq = match source {
            crate::IrqRef::Virq(virq) => virq,
            crate::IrqRef::Domain(domain, hwirq) => {
                if let Some(line) = self
                    .with_domain_index(domain, |index| index.get(&hwirq))
                    .flatten()
                {
                    return Some((line.logical_irq()?, Some(line)));
                }
                crate::domain::domain(domain)?.resolve_unmapped(hwirq)?
            }
        };
        Some((virq, self.descs.get(&virq)))
    }

    #[cfg(unittest)]
    pub(in crate::runtime) fn translated_hwirq(
        &self,
        domain: IrqDomainId,
        hwirq: Hwirq,
    ) -> Option<Virq> {
        self.domain_virq(domain, hwirq)
    }

    #[cfg(unittest)]
    pub(in crate::runtime) fn set_next_virq_for_tests(&mut self, next_virq: Virq) {
        self.next_virq = next_virq;
    }

    #[cfg(unittest)]
    pub(in crate::runtime) const fn next_virq_for_tests(&self) -> Virq {
        self.next_virq
    }

    pub(in crate::runtime) fn stored_desc(&self, virq: Virq) -> Option<IrqDesc> {
        self.descs.get(&virq).map(|line| line.lock().desc)
    }

    /// Publishes the prepared domain binding into the domain's own index.
    ///
    /// The caller validated the domain and both publication capacities while
    /// holding registry exclusion.
    fn publish_domain_mapping(
        &mut self,
        domain: IrqDomainId,
        line: &Arc<IrqLine>,
        prepared: &mut PreparedPublication,
    ) {
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: domain validation and both \
                      publication capacities completed before either insertion"
        )]
        self.with_domain_index_mut(domain, |index| {
            index.publish(
                line.clone(),
                prepared.domain.take().expect("prepared domain insertion"),
            );
        })
        .expect("validated domain index");
    }

    fn domain_virq(&self, domain: IrqDomainId, hwirq: Hwirq) -> Option<Virq> {
        self.with_domain_index(domain, |index| index.get(&hwirq))
            .flatten()?
            .logical_irq()
    }

    #[cfg(unittest)]
    fn mapping_for_virq(&self, virq: Virq) -> Option<(IrqDomainId, Hwirq)> {
        let line = self.descs.get(&virq)?;
        let desc = line.lock().desc;
        let domain = desc.domain?;
        (self.domain_virq(domain, desc.hwirq) == Some(virq)).then_some((domain, desc.hwirq))
    }

    /// Synthetic quiescent fixture teardown, not production RCU retirement.
    #[cfg(unittest)]
    pub(in crate::runtime) fn remove_if_unused(&mut self, virq: Virq) -> Option<IrqDesc> {
        let line = self.descs.get(&virq)?;
        let mut entry = line.lock();
        if entry.desc.domain.is_some() || entry.is_msi() || !entry.is_unused() {
            return None;
        }
        entry.is_retired = true;
        let desc = entry.desc;
        drop(entry);
        self.descs.remove(&virq);
        Some(desc)
    }

    #[cfg(unittest)]
    pub(in crate::runtime) fn remove_test_line(&mut self, virq: Virq) -> Option<Arc<IrqLine>> {
        let line = self.descs.get(&virq)?;
        let desc = {
            let mut entry = line.lock();
            entry.is_retired = true;
            entry.desc
        };
        if let Some(domain) = desc.domain {
            self.with_domain_index_mut(domain, |index| {
                index.remove(&desc.hwirq);
            });
        }
        self.descs.remove(&virq)
    }

    #[cfg(unittest)]
    pub(in crate::runtime) fn remove_msi_if_unused(&mut self, virq: Virq) -> Option<Hwirq> {
        let line = self.descs.get(&virq)?;
        let entry = line.lock();
        if !entry.is_msi() || !entry.is_unused() {
            return None;
        }
        let hwirq = entry.desc.hwirq;
        drop(entry);
        self.remove_test_line(virq)?;
        Some(hwirq)
    }

    /// Caller preflight precedes exclusion. Close admission before either
    /// index is withdrawn and return all retirement rights for unlocked submit.
    ///
    /// # Safety
    /// The caller successfully preflighted before registry exclusion and must
    /// retire the returned records after restoring context, before other callbacks.
    #[cfg(any(target_arch = "x86_64", unittest))]
    pub(in crate::runtime) unsafe fn withdraw_msi(
        &mut self,
        virq: Virq,
    ) -> Option<(
        Arc<IrqLine>,
        MsiReservation,
        WithdrawnIndexes,
        intrusive_collections::LinkedList<RetirementAdapter>,
    )> {
        let line = self.descs.get(&virq)?;
        let mut entry = line.lock();
        if !entry.is_msi()
            || !entry.is_unused()
            || entry.runtime.is_enabled
            || entry.msi_reservation.as_ref()?.hwirq() != entry.desc.hwirq
        {
            return None;
        }
        let prepared = entry.msi_withdrawal.take()?;
        entry.is_retired = true;
        let retirements = entry.take_retirements();
        let desc = entry.desc;
        let reservation = entry.msi_reservation.take()?;
        // SAFETY: caller preflight and registry exclusion span validation and
        // both withdrawals; every pending record is returned for unlocked submit.
        let domain = if let Some(domain) = desc.domain {
            self.with_domain_index_mut(domain, |index| {
                // SAFETY: the caller's preflight and registry exclusion cover
                // this withdrawal; its pending record is returned to the caller.
                unsafe { index.withdraw(desc.hwirq, prepared.domain) }
            })
            .flatten()
        } else {
            None
        };
        #[cfg(unittest)]
        self.observe_index_transition(&line, &entry);
        // SAFETY: caller preflight still spans this second withdrawal, and its
        // pending retirement is returned for submission outside exclusion.
        let logical = unsafe { self.descs.withdraw(virq, prepared.logical) };
        drop(entry);
        Some((
            line,
            reservation,
            WithdrawnIndexes { logical, domain },
            retirements,
        ))
    }
}

/// Resolves while the caller keeps the selected Line locked through its update.
///
/// The caller holds registry exclusion, selects the existing Line or an
/// unpublished candidate, and passes that Line's locked state. This function
/// never reacquires a Line lock. Rejected validation does not change metadata
/// or mappings; allocating a fresh dynamic identity may advance the allocator.
pub(in crate::runtime) fn resolve_prepared_locked(
    state: &mut IrqState,
    spec: IrqSpec,
    line: &Arc<IrqLine>,
    entry: &mut IrqStateDesc,
    prepared: &mut PreparedPublication,
) -> Result<IrqDesc, IrqDescError> {
    validate_resolution_domain(spec)?;
    let resolution = state.plan_resolution(spec)?;
    let resolved = state.commit_resolution(resolution, line, entry, prepared)?;
    Ok(resolved.into_desc())
}

pub(in crate::runtime) fn validate_resolution_domain(spec: IrqSpec) -> Result<(), IrqDescError> {
    let domain_id = match spec {
        IrqSpec::PlainVirq(_) => None,
        IrqSpec::Desc(desc) => desc.domain,
    };
    if let Some(domain_id) = domain_id
        && crate::domain::domain(domain_id).is_none()
    {
        return Err(IrqDescError::UnknownDomain { domain: domain_id });
    }
    Ok(())
}

#[cfg(unittest)]
pub(in crate::runtime) fn try_resolve_and_publish_locked(
    state: &mut IrqState,
    spec: IrqSpec,
    line: &Arc<IrqLine>,
    entry: &mut IrqStateDesc,
) -> Result<IrqDesc, IrqDescError> {
    let virq = line.logical_irq().unwrap_or(entry.desc.hwirq);
    let mapping = match spec {
        IrqSpec::Desc(desc) => desc.domain.map(|domain| (domain, desc.hwirq)),
        _ => None,
    };
    let mut prepared = PreparedPublication::new(virq, mapping, true)?;
    resolve_prepared_locked(state, spec, line, entry, &mut prepared)
}

#[cfg(any(target_arch = "x86_64", unittest))]
#[expect(
    clippy::expect_used,
    reason = "CG-021-no-panic-on-reachable-failure: a rejected private candidate retains its \
              reservation; successful resolution always assigns a logical IRQ"
)]
pub(crate) fn publish_msi_reservation(
    desc: IrqDesc,
    reservation: MsiReservation,
) -> Result<Virq, (IrqDescError, MsiReservation)> {
    let Some(withdrawal) = PreparedWithdrawal::new(desc.hwirq) else {
        return Err((IrqDescError::NoMemory, reservation));
    };
    let mut entry = IrqStateDesc::new(desc);
    entry.msi_reservation = Some(reservation);
    entry.msi_withdrawal = Some(withdrawal);
    let line = Arc::new(IrqLine::new(entry));
    let result = (|| {
        validate_resolution_domain(IrqSpec::Desc(desc))?;
        let virq = IRQ_STATE.lock().alloc_virq()?;
        let mut prepared =
            PreparedPublication::new(virq, desc.domain.map(|domain| (domain, desc.hwirq)), true)?;
        let resolution = IrqResolution {
            virq,
            offered: Some(desc.with_virq(virq)),
        };
        let mut state = IRQ_STATE.lock();
        state
            .commit_resolution(resolution, &line, &mut line.lock(), &mut prepared)
            .map(ResolvedIrqDesc::into_desc)
    })();
    match result {
        Ok(desc) => Ok(desc.logical_irq().expect("MSI publication assigned virq")),
        Err(error) => {
            let reservation = line
                .lock()
                .msi_reservation
                .take()
                .expect("private MSI candidate retains its reservation");
            Err((error, reservation))
        }
    }
}

#[cfg(any(target_arch = "x86_64", unittest))]
pub(crate) fn free_msi_if_unused(
    virq: Virq,
    free_backend_vector_fn: impl FnOnce(MsiReservation),
) -> bool {
    if krcu::preflight_callback_submission().is_err() {
        return false;
    }
    let removed = {
        let mut state = IRQ_STATE.lock();
        // SAFETY: preflight preceded exclusion; submission follows immediately
        // after restoring that context, before the backend/user callback.
        unsafe { state.withdraw_msi(virq) }
    };
    let Some((line, reservation, pending, retirements)) = removed else {
        return false;
    };
    pending.retire();
    free_backend_vector_fn(reservation);
    notify_retirements(virq, retirements);
    drop(line);
    true
}

// Mirror production withdrawal/submission on a private registry. Backend
// release runs only after submission, never under the registry/Line locks.
#[cfg(unittest)]
fn release_unused_msi(
    state: &mut IrqState,
    virq: Virq,
    free_backend_vector_fn: impl FnOnce(&IrqState, MsiReservation),
) -> Option<Arc<IrqLine>> {
    krcu::preflight_callback_submission().ok()?;
    // SAFETY: this private-registry test helper preflights outside exclusion,
    // then submits immediately before invoking the observation callback.
    let (line, reservation, pending, retirements) = unsafe { state.withdraw_msi(virq) }?;
    pending.retire();
    free_backend_vector_fn(state, reservation);
    notify_retirements(virq, retirements);
    Some(line)
}

unittest::path_mod_test!(tests_line_admission, "../../../ut-tests/line_admission.rs");
unittest::path_mod_test!(
    tests_msi_reservation,
    "../../../ut-tests/msi_reservation.rs"
);

#[cfg(unittest)]
pub use tests_msi_reservation::RcuIndexProbe;
unittest::path_mod_test!(
    tests_mapping_lifetime,
    "../../../ut-tests/mapping_lifetime.rs"
);
unittest::path_mod_test!(
    tests_index_transactions,
    "../../../ut-tests/index_transactions.rs"
);

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IRQ descriptor and wake-subscription state.

use alloc::collections::{BTreeMap, btree_map::Entry};

use kspin::SpinNoIrq;

use crate::{
    Hwirq, IrqDesc, IrqDescError, IrqDomainId, IrqFlags, IrqSpec, Virq, action::IrqAction,
};

/// IRQ control-plane aggregate.
///
/// All descriptor, handler, wakeup, and `(domain, hwirq) -> virq` namespace
/// changes go through this lock. Dispatch may briefly snapshot an action from
/// this state, but hardware IRQ translation on the hot path consumes the
/// immutable reverse maps published by `domain.rs` instead of walking these
/// mutable maps directly.
pub(super) static IRQ_STATE: SpinNoIrq<IrqState> = SpinNoIrq::new(IrqState::new());
pub const DYNAMIC_VIRQ_BASE: Virq = 4096;

pub(super) type WakeHandler = fn(usize);

/// Result of resolving one caller-provided IRQ descriptor.
///
/// This is the handoff object between the locked control-plane mutation and
/// the publication step. Keeping the publication target here makes mapping
/// changes explicit instead of hiding them behind a global dirty flag. The
/// snapshot field is deliberately separate from `IrqDesc::domain`: a
/// descriptor may belong to a domain without inserting a new mapping, and only
/// newly inserted mappings require publishing a new lock-free reverse-map
/// snapshot.
pub(super) struct ResolvedIrqDesc {
    desc: IrqDesc,
    snapshot_domain_to_publish: Option<IrqDomainId>,
}

impl ResolvedIrqDesc {
    const fn new(desc: IrqDesc, snapshot_domain_to_publish: Option<IrqDomainId>) -> Self {
        Self {
            desc,
            snapshot_domain_to_publish,
        }
    }

    const fn snapshot_domain_to_publish(&self) -> Option<IrqDomainId> {
        self.snapshot_domain_to_publish
    }

    pub(super) const fn into_desc(self) -> IrqDesc {
        self.desc
    }
}

/// Control-plane state owned by one IRQ domain.
///
/// This table is the mutable build source for the immutable reverse-map
/// snapshot published by `domain.rs`. It is not read by the hardirq data path.
struct IrqDomainState {
    hwirq_to_virq: BTreeMap<Hwirq, Virq>,
}

impl IrqDomainState {
    const fn new() -> Self {
        Self {
            hwirq_to_virq: BTreeMap::new(),
        }
    }

    fn virq_for_hwirq(&self, hwirq: Hwirq) -> Option<Virq> {
        self.hwirq_to_virq.get(&hwirq).copied()
    }

    fn insert_mapping(&mut self, hwirq: Hwirq, virq: Virq) -> bool {
        if let Entry::Vacant(entry) = self.hwirq_to_virq.entry(hwirq) {
            entry.insert(virq);
            return true;
        }
        false
    }

    fn remove_mapping(&mut self, hwirq: Hwirq) -> bool {
        self.hwirq_to_virq.remove(&hwirq).is_some()
    }

    fn is_empty(&self) -> bool {
        self.hwirq_to_virq.is_empty()
    }

    fn entries(&self) -> impl Iterator<Item = (Hwirq, Virq)> + '_ {
        self.hwirq_to_virq
            .iter()
            .map(|(&hwirq, &virq)| (hwirq, virq))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WakeupMode {
    Persistent,
    OneShot,
}

#[derive(Clone, Copy)]
pub(super) struct WakeSubscription {
    pub(super) mode: WakeupMode,
    pub(super) armed: bool,
    pub(super) handler: WakeHandler,
}

pub(super) struct IrqDispatchSnapshot {
    pub(super) desc: IrqDesc,
    pub(super) regular_action: Option<IrqAction>,
    pub(super) wake_subscription: Option<WakeSubscription>,
    pub(super) has_regular_action: bool,
}

#[derive(Clone)]
pub(super) struct IrqDescRuntimeState {
    regular_action: Option<IrqAction>,
    wake_subscription: Option<WakeSubscription>,
    generation: u64,
    is_msi: bool,
    shared_action_count: usize,
    oneshot_mask_pending: bool,
}

impl IrqDescRuntimeState {
    const fn new(desc: IrqDesc) -> Self {
        Self {
            regular_action: None,
            wake_subscription: None,
            generation: 0,
            is_msi: desc.flags.contains(IrqFlags::MSI),
            shared_action_count: 0,
            oneshot_mask_pending: false,
        }
    }

    fn remember_desc(&mut self, desc: IrqDesc) {
        self.is_msi = desc.flags.contains(IrqFlags::MSI);
    }

    fn bump_generation(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    fn install_regular_action(&mut self, action: IrqAction) -> bool {
        if self.regular_action.is_some() {
            return false;
        }
        self.regular_action = Some(action);
        self.shared_action_count = 1;
        self.bump_generation();
        true
    }

    fn take_regular_action(&mut self) -> Option<IrqAction> {
        let action = self.regular_action.take();
        if action.is_some() {
            self.shared_action_count = 0;
            self.oneshot_mask_pending = false;
            self.bump_generation();
        }
        action
    }

    fn install_wake_subscription(&mut self, subscription: WakeSubscription) -> bool {
        if self.regular_action.is_none() {
            return false;
        }
        self.wake_subscription = Some(subscription);
        self.bump_generation();
        true
    }

    fn take_wake_subscription(&mut self) -> Option<WakeSubscription> {
        let subscription = self.wake_subscription.take();
        if subscription.is_some() {
            self.bump_generation();
        }
        subscription
    }

    fn snapshot_dispatch_wake_subscription(&mut self) -> Option<WakeSubscription> {
        match self.wake_subscription {
            Some(subscription) if subscription.mode == WakeupMode::Persistent => Some(subscription),
            Some(subscription) if subscription.armed => {
                self.wake_subscription = None;
                self.bump_generation();
                Some(subscription)
            }
            Some(_) => {
                self.wake_subscription = None;
                self.bump_generation();
                None
            }
            None => None,
        }
    }

    const fn has_regular_action(&self) -> bool {
        self.regular_action.is_some()
    }

    #[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
    const fn is_msi(&self) -> bool {
        self.is_msi
    }

    const fn is_unused(&self) -> bool {
        self.regular_action.is_none() && self.wake_subscription.is_none()
    }

    #[cfg(unittest)]
    const fn has_wake_subscription(&self) -> bool {
        self.wake_subscription.is_some()
    }
}

#[derive(Clone)]
pub(super) struct IrqStateDesc {
    pub(super) desc: IrqDesc,
    runtime: IrqDescRuntimeState,
}

impl IrqStateDesc {
    pub(super) const fn new(desc: IrqDesc) -> Self {
        Self {
            desc,
            runtime: IrqDescRuntimeState::new(desc),
        }
    }

    pub(super) fn update_desc(&mut self, desc: IrqDesc) {
        if self.desc != desc {
            self.runtime.bump_generation();
        }
        self.desc = desc;
        self.runtime.remember_desc(desc);
    }

    pub(super) fn remove_flags(&mut self, flags: IrqFlags) {
        self.update_desc(IrqDesc {
            flags: self.desc.flags - flags,
            ..self.desc
        });
    }

    pub(super) fn install_regular_action(&mut self, action: IrqAction) -> bool {
        self.runtime.install_regular_action(action)
    }

    pub(super) fn take_regular_action(&mut self) -> Option<IrqAction> {
        self.runtime.take_regular_action()
    }

    pub(super) fn has_regular_action(&self) -> bool {
        self.runtime.has_regular_action()
    }

    pub(super) fn install_wake_subscription(&mut self, subscription: WakeSubscription) -> bool {
        self.runtime.install_wake_subscription(subscription)
    }

    pub(super) fn take_wake_subscription(&mut self) -> Option<WakeSubscription> {
        self.runtime.take_wake_subscription()
    }

    pub(super) fn snapshot_dispatch(&mut self) -> IrqDispatchSnapshot {
        let regular_action = self.runtime.regular_action.clone();
        let has_regular_action = regular_action.is_some();
        let wake_subscription = if regular_action
            .as_ref()
            .is_none_or(IrqAction::is_currently_dispatchable)
        {
            self.runtime.snapshot_dispatch_wake_subscription()
        } else {
            self.runtime.wake_subscription
        };
        IrqDispatchSnapshot {
            desc: self.desc,
            regular_action,
            wake_subscription,
            has_regular_action,
        }
    }

    #[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
    pub(super) fn is_msi(&self) -> bool {
        self.runtime.is_msi()
    }

    pub(super) fn is_unused(&self) -> bool {
        self.runtime.is_unused()
    }

    #[cfg(unittest)]
    pub(super) fn test_with_runtime(
        desc: IrqDesc,
        regular_action: Option<IrqAction>,
        wake_subscription: Option<WakeSubscription>,
    ) -> Self {
        let mut state = Self::new(desc);
        if let Some(action) = regular_action {
            assert!(state.install_regular_action(action));
        }
        if let Some(subscription) = wake_subscription {
            assert!(state.install_wake_subscription(subscription));
        }
        state
    }

    #[cfg(unittest)]
    pub(super) fn has_wake_subscription(&self) -> bool {
        self.runtime.has_wake_subscription()
    }

    #[cfg(unittest)]
    pub(super) const fn generation_for_tests(&self) -> u64 {
        self.runtime.generation
    }
}

pub(super) struct IrqState {
    /// Primary OS-visible IRQ namespace.
    ///
    /// `virq` is the stable key used by registration, unregister, descriptor
    /// queries, and normal subscriber dispatch.
    pub(super) descs: BTreeMap<Virq, IrqStateDesc>,
    /// Per-domain control-plane state.
    ///
    /// Each domain owns its mutable `hwirq -> virq` build table. This is not
    /// the hardirq lookup structure. Whenever a new mapping is inserted, the
    /// affected domain publishes an immutable reverse-map snapshot for
    /// lock-free dispatch.
    domain_states: BTreeMap<IrqDomainId, IrqDomainState>,
    /// Next dynamically allocated OS-visible IRQ number.
    ///
    /// Caller-provided `virq` values occupy the explicit OS-visible namespace;
    /// dynamically assigned descriptors start here so they do not collide with
    /// ordinary low platform line numbers.
    next_virq: Virq,
}

impl IrqState {
    const fn new() -> Self {
        Self {
            descs: BTreeMap::new(),
            domain_states: BTreeMap::new(),
            next_virq: DYNAMIC_VIRQ_BASE,
        }
    }

    fn alloc_virq(&mut self) -> Result<Virq, IrqDescError> {
        loop {
            let virq = self.next_virq;
            self.next_virq = self
                .next_virq
                .checked_add(1)
                .ok_or(IrqDescError::VirqExhausted { next: virq })?;
            if !self.descs.contains_key(&virq) && self.mapping_for_virq(virq).is_none() {
                return Ok(virq);
            }
        }
    }

    pub(super) fn try_resolve_desc(
        &mut self,
        mut desc: IrqDesc,
    ) -> Result<ResolvedIrqDesc, IrqDescError> {
        let (virq, mapping_to_insert) = if let Some(virq) = desc.logical_irq() {
            if let Some(existing) = self.descs.get(&virq) {
                existing.desc.try_merge(desc)?;
            }
            if let Some(domain) = desc.domain {
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
                (
                    virq,
                    existing_virq
                        .is_none()
                        .then_some((domain, desc.hwirq, virq)),
                )
            } else {
                (virq, None)
            }
        } else if let Some(domain) = desc.domain {
            if let Some(virq) = self.domain_virq(domain, desc.hwirq) {
                (virq, None)
            } else {
                let virq = self.alloc_virq()?;
                (virq, Some((domain, desc.hwirq, virq)))
            }
        } else {
            (desc.hwirq, None)
        };
        desc = desc.with_virq(virq);
        let stored_desc = if let Some(existing) = self.descs.get(&virq) {
            existing.desc.try_merge(desc)?
        } else {
            desc
        };
        let mut snapshot_domain_to_publish = None;
        if let Some((domain, hwirq, virq)) = mapping_to_insert {
            if let Some((existing_domain, existing_hwirq)) = self.mapping_for_virq(virq) {
                return Err(IrqDescError::VirqMappingConflict {
                    virq,
                    existing_domain,
                    existing_hwirq,
                    newer_domain: domain,
                    newer_hwirq: hwirq,
                });
            }
            let inserted = self.domain_state_mut(domain).insert_mapping(hwirq, virq);
            debug_assert!(inserted, "domain mapping insertion was prevalidated");
            if inserted {
                snapshot_domain_to_publish = Some(domain);
            }
        }
        self.descs
            .entry(virq)
            .and_modify(|state| state.update_desc(stored_desc))
            .or_insert_with(|| IrqStateDesc::new(stored_desc));
        Ok(ResolvedIrqDesc::new(
            stored_desc,
            snapshot_domain_to_publish,
        ))
    }

    pub(super) fn try_resolve_spec(
        &mut self,
        spec: IrqSpec,
    ) -> Result<ResolvedIrqDesc, IrqDescError> {
        match spec {
            IrqSpec::PlainVirq(virq) => {
                Ok(ResolvedIrqDesc::new(self.resolve_plain_virq(virq), None))
            }
            IrqSpec::Desc(desc) => self.try_resolve_desc(desc),
        }
    }

    pub(super) fn lookup_virq(&self, spec: IrqSpec) -> Option<Virq> {
        match spec {
            IrqSpec::PlainVirq(virq) => Some(virq),
            IrqSpec::Desc(desc) => desc.logical_irq().or_else(|| {
                desc.domain
                    .and_then(|domain| self.domain_virq(domain, desc.hwirq))
            }),
        }
    }

    #[cfg(unittest)]
    pub(super) fn translated_hwirq(&self, domain: IrqDomainId, hwirq: Hwirq) -> Option<Virq> {
        self.domain_virq(domain, hwirq)
    }

    #[cfg(unittest)]
    pub(super) fn set_next_virq_for_tests(&mut self, next_virq: Virq) {
        self.next_virq = next_virq;
    }

    #[cfg(unittest)]
    pub(super) const fn next_virq_for_tests(&self) -> Virq {
        self.next_virq
    }

    pub(super) fn stored_desc(&self, virq: Virq) -> Option<IrqDesc> {
        self.descs.get(&virq).map(|state| state.desc)
    }

    fn resolve_plain_virq(&mut self, virq: Virq) -> IrqDesc {
        if let Some(existing) = self.stored_desc(virq) {
            return existing;
        }
        let desc = IrqDesc::from_virq(virq);
        self.descs
            .entry(virq)
            .or_insert_with(|| IrqStateDesc::new(desc));
        desc
    }

    fn domain_virq(&self, domain: IrqDomainId, hwirq: Hwirq) -> Option<Virq> {
        self.domain_states
            .get(&domain)
            .and_then(|state| state.virq_for_hwirq(hwirq))
    }

    fn mapping_for_virq(&self, virq: Virq) -> Option<(IrqDomainId, Hwirq)> {
        self.domain_states.iter().find_map(|(&domain, state)| {
            state
                .entries()
                .find_map(|(hwirq, mapped_virq)| (mapped_virq == virq).then_some((domain, hwirq)))
        })
    }

    fn domain_state_mut(&mut self, domain: IrqDomainId) -> &mut IrqDomainState {
        self.domain_states
            .entry(domain)
            .or_insert_with(IrqDomainState::new)
    }

    fn domain_mapping_entries(
        &self,
        domain_id: IrqDomainId,
    ) -> impl Iterator<Item = (Hwirq, Virq)> + '_ {
        self.domain_states
            .get(&domain_id)
            .into_iter()
            .flat_map(IrqDomainState::entries)
    }

    pub(super) fn remove_if_unused(&mut self, virq: Virq) {
        self.remove_if_unused_inner(virq, false);
    }

    fn remove_if_unused_inner(&mut self, virq: Virq, remove_msi: bool) -> Option<Hwirq> {
        let entry = self.descs.get(&virq)?;
        if entry.desc.flags.contains(IrqFlags::MSI) && !remove_msi {
            return None;
        }
        if !entry.is_unused() {
            return None;
        }
        let hwirq = entry.desc.hwirq;
        if remove_msi
            && let Some(domain) = entry.desc.domain
            && let Some(domain_state) = self.domain_states.get_mut(&domain)
        {
            domain_state.remove_mapping(hwirq);
            if domain_state.is_empty() {
                self.domain_states.remove(&domain);
            }
        }
        self.descs.remove(&virq);
        Some(hwirq)
    }

    pub(super) fn remove_if_unused_with_desc(&mut self, virq: Virq) -> Option<IrqDesc> {
        let entry = self.descs.get(&virq)?;
        if entry.desc.flags.contains(IrqFlags::MSI) || !entry.is_unused() {
            return None;
        }
        let desc = entry.desc;
        self.descs.remove(&virq);
        Some(desc)
    }

    #[cfg(any(target_arch = "x86_64", unittest))]
    pub(super) fn remove_msi_if_unused(&mut self, virq: Virq) -> Option<Hwirq> {
        let entry = self.descs.get(&virq)?;
        if !entry.desc.flags.contains(IrqFlags::MSI) {
            return None;
        }
        let domain = entry.desc.domain;
        let hwirq = self.remove_if_unused_inner(virq, true)?;
        if let Some(domain) = domain {
            let published =
                crate::domain::publish_snapshot(domain, self.domain_mapping_entries(domain));
            if !published {
                warn!(
                    "remove_msi_if_unused: unregistered irq domain {domain:?}; removed MSI \
                     mapping will not be reflected on the data path"
                );
            }
        }
        Some(hwirq)
    }

    #[cfg(target_arch = "x86_64")]
    pub(super) fn is_unused(&self, virq: Virq) -> bool {
        self.descs.get(&virq).is_some_and(IrqStateDesc::is_unused)
    }

    #[cfg(target_arch = "x86_64")]
    pub(super) fn is_msi(&self, virq: Virq) -> bool {
        self.descs.get(&virq).is_some_and(IrqStateDesc::is_msi)
    }
}

pub(super) fn try_resolve_and_publish(
    state: &mut IrqState,
    spec: IrqSpec,
) -> Result<IrqDesc, IrqDescError> {
    let domain_id = match spec {
        IrqSpec::PlainVirq(_) => None,
        IrqSpec::Desc(desc) => desc.domain,
    };
    if let Some(domain_id) = domain_id
        && crate::domain::domain(domain_id).is_none()
    {
        return Err(IrqDescError::UnknownDomain { domain: domain_id });
    }
    let resolved = state.try_resolve_spec(spec)?;
    if let Some(domain_id) = resolved.snapshot_domain_to_publish() {
        let published =
            crate::domain::publish_snapshot(domain_id, state.domain_mapping_entries(domain_id));
        debug_assert!(published);
    }
    Ok(resolved.into_desc())
}

#[cfg(target_arch = "x86_64")]
pub(crate) fn free_msi_if_unused(
    virq: Virq,
    free_backend_vector_fn: impl FnOnce(Hwirq) -> bool,
) -> bool {
    let mut state = IRQ_STATE.lock();
    let Some(desc) = state.stored_desc(virq) else {
        return false;
    };
    if !state.is_msi(virq) {
        return false;
    }
    if !state.is_unused(virq) {
        warn!(
            "refusing to free MSI IRQ {virq} while a handler or wake subscription is still \
             registered"
        );
        return false;
    }

    let hwirq = desc.hwirq;
    if !free_backend_vector_fn(hwirq) {
        return false;
    }

    state.remove_msi_if_unused(virq).is_some()
}

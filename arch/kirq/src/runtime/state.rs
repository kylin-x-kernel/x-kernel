// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IRQ descriptor and action state.

use alloc::{
    collections::{BTreeMap, btree_map::Entry},
    sync::Arc,
};
use core::array;

use kpoll::{Completion, PollSet};
use kspin::SpinNoIrq;

use crate::{
    Hwirq, IrqDesc, IrqDescError, IrqDomainId, IrqFlags, IrqSpec, Virq,
    action::{IrqAction, IrqActionToken},
};

/// IRQ control-plane aggregate.
///
/// All descriptor, handler, and `(domain, hwirq) -> virq` namespace changes go
/// through this lock. Dispatch may briefly snapshot an action from this state,
/// but hardware IRQ translation on the hot path consumes the immutable reverse
/// maps published by `domain.rs` instead of walking these mutable maps directly.
pub(super) static IRQ_STATE: SpinNoIrq<IrqState> = SpinNoIrq::new(IrqState::new());
pub const DYNAMIC_VIRQ_BASE: Virq = 4096;
pub(super) const MAX_IRQ_ACTIONS: usize = 4;

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

pub(super) struct IrqActionSnapshot {
    pub(super) actions: [Option<IrqAction>; MAX_IRQ_ACTIONS],
    pub(super) action_count: usize,
}

impl IrqActionSnapshot {
    /// Returns whether this snapshot contains hardirq-stage primary actions.
    ///
    /// Only primary actions leave `IRQ_STATE` and run later outside the lock, so
    /// only they require an in-flight dispatch guard.
    pub(super) fn has_primary_actions(&self) -> bool {
        self.action_count != 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct IrqPlatformPlan {
    pub(super) desc: IrqDesc,
    pub(super) configure: bool,
    pub(super) enable: Option<bool>,
}

impl IrqPlatformPlan {
    pub(super) const fn none(desc: IrqDesc) -> Self {
        Self {
            desc,
            configure: false,
            enable: None,
        }
    }
}

pub(super) struct IrqDescRuntimeState {
    actions: [Option<IrqAction>; MAX_IRQ_ACTIONS],
    action_count: usize,
    generation: u64,
    configured_generation: Option<u64>,
    is_enabled: bool,
    disable_depth: usize,
    in_flight: usize,
    in_flight_zero: Arc<Completion>,
    teardown_depth: usize,
    is_msi: bool,
    oneshot_mask_pending: bool,
    next_shared_action_token: usize,
}

impl IrqDescRuntimeState {
    fn new(desc: IrqDesc) -> Self {
        let in_flight_zero = Arc::new(Completion::new());
        in_flight_zero.complete_all();
        Self {
            actions: array::from_fn(|_| None),
            action_count: 0,
            generation: 0,
            configured_generation: None,
            is_enabled: false,
            disable_depth: 0,
            in_flight: 0,
            in_flight_zero,
            teardown_depth: 0,
            is_msi: desc.flags.contains(IrqFlags::MSI),
            oneshot_mask_pending: false,
            next_shared_action_token: 1,
        }
    }

    fn remember_desc(&mut self, desc: IrqDesc) {
        self.is_msi = desc.flags.contains(IrqFlags::MSI);
    }

    fn bump_generation(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    fn prepare_auto_enable(&mut self, desc: IrqDesc) -> IrqPlatformPlan {
        self.disable_depth = 0;
        self.prepare_enable_at_depth_zero(desc, false)
    }

    fn prepare_register_disabled(&mut self, desc: IrqDesc) -> IrqPlatformPlan {
        if self.disable_depth == 0 {
            self.disable_depth = 1;
        }
        IrqPlatformPlan::none(desc)
    }

    fn prepare_enable_irq(
        &mut self,
        desc: IrqDesc,
        force_platform_enable: bool,
    ) -> IrqPlatformPlan {
        if self.disable_depth > 0 {
            self.disable_depth -= 1;
            if self.disable_depth > 0 {
                return IrqPlatformPlan::none(desc);
            }
        }
        self.prepare_enable_at_depth_zero(desc, force_platform_enable)
    }

    fn prepare_enable_at_depth_zero(
        &mut self,
        desc: IrqDesc,
        force_platform_enable: bool,
    ) -> IrqPlatformPlan {
        let configure = self.configured_generation != Some(self.generation);
        let enable = (force_platform_enable || !self.is_enabled).then_some(true);
        if configure {
            self.configured_generation = Some(self.generation);
        }
        if enable.is_some() {
            self.is_enabled = true;
        }
        IrqPlatformPlan {
            desc,
            configure,
            enable,
        }
    }

    fn prepare_reconfigure_if_stale(&mut self, desc: IrqDesc) -> IrqPlatformPlan {
        let configure = self.configured_generation != Some(self.generation);
        if configure {
            self.configured_generation = Some(self.generation);
        }
        IrqPlatformPlan {
            desc,
            configure,
            enable: None,
        }
    }

    fn prepare_disable_irq_nosync(&mut self, desc: IrqDesc) -> IrqPlatformPlan {
        let was_enabled_depth = self.disable_depth == 0;
        self.disable_depth = self.disable_depth.saturating_add(1);
        let enable = (was_enabled_depth && self.is_enabled).then_some(false);
        if enable.is_some() {
            self.is_enabled = false;
        }
        IrqPlatformPlan {
            desc,
            configure: false,
            enable,
        }
    }

    fn prepare_disable_if_no_actions(&mut self, desc: IrqDesc) -> Option<IrqPlatformPlan> {
        // Platform masking is based only on action ownership. Existing teardown
        // waiters or old in-flight snapshots must not keep an action-less line
        // enabled; they only delay descriptor cleanup.
        if self.action_count != 0 {
            return None;
        }
        self.disable_depth = self.disable_depth.max(1);
        let enable = self.is_enabled.then_some(false);
        if enable.is_some() {
            self.is_enabled = false;
        }
        Some(IrqPlatformPlan {
            desc,
            configure: false,
            enable,
        })
    }

    fn install_regular_action(&mut self, action: IrqAction) -> bool {
        if self.teardown_depth != 0 {
            return false;
        }
        if self.action_count != 0 {
            return false;
        }
        self.actions[0] = Some(action);
        self.action_count = 1;
        true
    }

    fn take_regular_action(&mut self) -> Option<IrqAction> {
        if self.action_count != 1 {
            return None;
        }
        if self.actions[0].as_ref().is_some_and(IrqAction::is_shared) {
            return None;
        }
        let action = self.actions[0].take();
        self.action_count = 0;
        self.oneshot_mask_pending = false;
        action
    }

    fn install_shared_action(
        &mut self,
        handler: crate::platform::Handler,
    ) -> Option<IrqActionToken> {
        if self.teardown_depth != 0 {
            return None;
        }
        if self.action_count >= MAX_IRQ_ACTIONS {
            return None;
        }
        if self
            .actions
            .iter()
            .flatten()
            .any(|action| !action.is_shared())
        {
            return None;
        }
        let token = IrqActionToken::new(self.next_shared_action_token);
        self.next_shared_action_token = self.next_shared_action_token.checked_add(1)?;
        let slot = self.actions.iter_mut().find(|slot| slot.is_none())?;
        *slot = Some(IrqAction::shared(token, handler));
        self.action_count += 1;
        Some(token)
    }

    fn take_action(&mut self, token: IrqActionToken) -> Option<IrqAction> {
        let action_index = self.actions.iter().position(|action| {
            action
                .as_ref()
                .is_some_and(|action| action.token() == token)
        })?;
        if !self.actions[action_index]
            .as_ref()
            .is_some_and(IrqAction::is_shared)
        {
            return None;
        }
        let action = self.actions[action_index].take();
        self.action_count -= 1;
        if self.action_count == 0 {
            self.oneshot_mask_pending = false;
        }
        action
    }

    const fn has_actions(&self) -> bool {
        self.action_count != 0
    }

    const fn action_count(&self) -> usize {
        self.action_count
    }

    fn begin_dispatch(&mut self) {
        if self.in_flight == 0 {
            self.in_flight_zero.reinit();
        }
        self.in_flight = self.in_flight.saturating_add(1);
    }

    fn finish_dispatch(&mut self) -> Option<PollSet> {
        if self.in_flight == 0 {
            warn!("IRQ dispatch in-flight underflow");
            None
        } else {
            self.in_flight -= 1;
            (self.in_flight == 0).then(|| self.in_flight_zero.complete_all_defer_wake())
        }
    }

    const fn in_flight(&self) -> usize {
        self.in_flight
    }

    fn in_flight_zero_completion(&self) -> Arc<Completion> {
        self.in_flight_zero.clone()
    }

    fn begin_teardown(&mut self) {
        self.teardown_depth = self.teardown_depth.saturating_add(1);
    }

    fn finish_teardown(&mut self) {
        if self.teardown_depth == 0 {
            warn!("IRQ teardown depth underflow");
        } else {
            self.teardown_depth -= 1;
        }
    }

    const fn is_teardown_in_progress(&self) -> bool {
        self.teardown_depth != 0
    }

    #[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
    const fn is_msi(&self) -> bool {
        self.is_msi
    }

    const fn is_unused(&self) -> bool {
        self.is_unused_ignoring_in_flight() && self.in_flight == 0
    }

    const fn is_unused_ignoring_in_flight(&self) -> bool {
        self.action_count == 0 && self.teardown_depth == 0
    }
}

pub(super) struct IrqStateDesc {
    pub(super) desc: IrqDesc,
    runtime: IrqDescRuntimeState,
}

impl IrqStateDesc {
    pub(super) fn new(desc: IrqDesc) -> Self {
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

    pub(super) fn install_shared_action(
        &mut self,
        handler: crate::platform::Handler,
    ) -> Option<IrqActionToken> {
        self.runtime.install_shared_action(handler)
    }

    pub(super) fn take_action(&mut self, token: IrqActionToken) -> Option<IrqAction> {
        self.runtime.take_action(token)
    }

    pub(super) fn has_actions(&self) -> bool {
        self.runtime.has_actions()
    }

    pub(super) fn action_count(&self) -> usize {
        self.runtime.action_count()
    }

    pub(super) fn snapshot_actions(&mut self) -> IrqActionSnapshot {
        let actions = self.runtime.actions.clone();
        IrqActionSnapshot {
            actions,
            action_count: self.runtime.action_count,
        }
    }

    pub(super) fn prepare_auto_enable(&mut self) -> IrqPlatformPlan {
        self.runtime.prepare_auto_enable(self.desc)
    }

    pub(super) fn prepare_register_disabled(&mut self) -> IrqPlatformPlan {
        self.runtime.prepare_register_disabled(self.desc)
    }

    pub(super) fn prepare_enable_irq(&mut self) -> IrqPlatformPlan {
        self.runtime.prepare_enable_irq(self.desc, false)
    }

    pub(super) fn prepare_legacy_enable(&mut self) -> IrqPlatformPlan {
        self.runtime.prepare_enable_irq(self.desc, true)
    }

    pub(super) fn prepare_reconfigure_if_stale(&mut self) -> IrqPlatformPlan {
        self.runtime.prepare_reconfigure_if_stale(self.desc)
    }

    pub(super) fn prepare_disable_irq_nosync(&mut self) -> IrqPlatformPlan {
        self.runtime.prepare_disable_irq_nosync(self.desc)
    }

    pub(super) fn prepare_disable_if_no_actions(&mut self) -> Option<IrqPlatformPlan> {
        self.runtime.prepare_disable_if_no_actions(self.desc)
    }

    pub(super) fn begin_dispatch(&mut self) {
        self.runtime.begin_dispatch();
    }

    pub(super) fn finish_dispatch(&mut self) -> Option<PollSet> {
        self.runtime.finish_dispatch()
    }

    pub(super) fn in_flight(&self) -> usize {
        self.runtime.in_flight()
    }

    pub(super) fn in_flight_zero_completion(&self) -> Arc<Completion> {
        self.runtime.in_flight_zero_completion()
    }

    pub(super) fn begin_teardown(&mut self) {
        self.runtime.begin_teardown();
    }

    pub(super) fn finish_teardown(&mut self) {
        self.runtime.finish_teardown();
    }

    pub(super) fn is_teardown_in_progress(&self) -> bool {
        self.runtime.is_teardown_in_progress()
    }

    #[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
    pub(super) fn is_msi(&self) -> bool {
        self.runtime.is_msi()
    }

    pub(super) fn is_unused(&self) -> bool {
        self.runtime.is_unused()
    }

    #[cfg(unittest)]
    pub(super) fn test_with_runtime(desc: IrqDesc, regular_action: Option<IrqAction>) -> Self {
        let mut state = Self::new(desc);
        if let Some(action) = regular_action {
            assert!(state.install_regular_action(action));
        }
        state
    }

    #[cfg(unittest)]
    pub(super) const fn generation_for_tests(&self) -> u64 {
        self.runtime.generation
    }

    #[cfg(unittest)]
    pub(super) const fn configured_generation_for_tests(&self) -> Option<u64> {
        self.runtime.configured_generation
    }

    #[cfg(unittest)]
    pub(super) const fn is_enabled_for_tests(&self) -> bool {
        self.runtime.is_enabled
    }

    #[cfg(unittest)]
    pub(super) const fn disable_depth_for_tests(&self) -> usize {
        self.runtime.disable_depth
    }

    #[cfg(unittest)]
    pub(super) const fn in_flight_for_tests(&self) -> usize {
        self.runtime.in_flight
    }

    #[cfg(unittest)]
    pub(super) const fn teardown_depth_for_tests(&self) -> usize {
        self.runtime.teardown_depth
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
    /// Reverse index for domain mappings keyed by OS-visible IRQ.
    ///
    /// This mirrors `domain_states` and makes the "one virq maps to at most
    /// one domain-local hwirq" invariant explicit instead of proving it through
    /// a full domain scan.
    virq_to_mapping: BTreeMap<Virq, (IrqDomainId, Hwirq)>,
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
            virq_to_mapping: BTreeMap::new(),
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
                let old_mapping = self.virq_to_mapping.insert(virq, (domain, hwirq));
                debug_assert_eq!(old_mapping, None, "virq mapping insertion was prevalidated");
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
        self.virq_to_mapping.get(&virq).copied()
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

    pub(super) fn remove_if_unused(&mut self, virq: Virq) -> Option<IrqDesc> {
        self.remove_if_unused_inner(virq, false)
    }

    fn remove_if_unused_inner(&mut self, virq: Virq, remove_msi: bool) -> Option<IrqDesc> {
        let entry = self.descs.get(&virq)?;
        if entry.desc.flags.contains(IrqFlags::MSI) && !remove_msi {
            return None;
        }
        if !entry.is_unused() {
            return None;
        }
        let desc = entry.desc;
        if remove_msi
            && let Some(domain) = desc.domain
            && let Some(domain_state) = self.domain_states.get_mut(&domain)
        {
            if domain_state.remove_mapping(desc.hwirq) {
                self.virq_to_mapping.remove(&virq);
            }
            if domain_state.is_empty() {
                self.domain_states.remove(&domain);
            }
        }
        self.descs.remove(&virq);
        Some(desc)
    }

    pub(super) fn remove_if_unused_with_desc(&mut self, virq: Virq) -> Option<IrqDesc> {
        self.remove_if_unused_inner(virq, false)
    }

    #[cfg(any(target_arch = "x86_64", unittest))]
    pub(super) fn remove_msi_if_unused(&mut self, virq: Virq) -> Option<Hwirq> {
        let entry = self.descs.get(&virq)?;
        if !entry.desc.flags.contains(IrqFlags::MSI) {
            return None;
        }
        let domain = entry.desc.domain;
        let desc = self.remove_if_unused_inner(virq, true)?;
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
        Some(desc.hwirq)
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

    let removed = state.remove_msi_if_unused(virq).is_some();
    drop(state);
    if removed {
        super::notify::remove_irq_waiters(virq);
    }
    removed
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IRQ descriptor and action state.

use alloc::{
    collections::{BTreeMap, btree_map::Entry},
    sync::Arc,
};
use core::array;

use intrusive_collections::intrusive_adapter;

#[cfg(any(target_arch = "x86_64", unittest))]
use crate::backend::msi::MsiReservation;
#[path = "state_local.rs"]
mod local;

use kcompletion::{Completion, DeferredWake};
use klazy::Once;
use kspin::{SpinNoIrq, SpinNoIrqGuard, static_lock};

use super::unhandled::{UnhandledIrqReport, UnhandledIrqState};
use crate::{
    Hwirq, IrqDesc, IrqDescError, IrqDomainId, IrqFlags, IrqSpec, Virq,
    action::{IrqAction, IrqActionToken},
};

static_lock! {
    /// IRQ control-plane aggregate.
    ///
    /// All descriptor, handler, and `(domain, hwirq) -> virq` namespace changes go
    /// through this lock. Dispatch may briefly snapshot an action from this state,
    /// but hardware IRQ translation on the hot path consumes the immutable reverse
    /// maps published by `domain.rs` instead of walking these mutable maps directly.
    pub(super) static IRQ_STATE: SpinNoIrq<IrqState> = SpinNoIrq::new(IrqState::new());
}
pub const DYNAMIC_VIRQ_BASE: Virq = 4096;
pub(super) const MAX_IRQ_ACTIONS: usize = 4;

/// Result of resolving one caller-provided IRQ descriptor.
///
/// This is the handoff object between the locked control-plane mutation and
/// the allocation-free publication step. A prepared owner is present only for
/// newly inserted mappings; existing mappings need no allocation. Discarding
/// this result without publication is reserved for isolated test registries.
pub(super) struct ResolvedIrqDesc {
    desc: IrqDesc,
    publication: Option<crate::domain::PreparedPublication>,
}

impl ResolvedIrqDesc {
    fn new(desc: IrqDesc, publication: Option<crate::domain::PreparedPublication>) -> Self {
        Self { desc, publication }
    }

    #[cfg(unittest)]
    pub(super) fn into_desc(self) -> IrqDesc {
        self.desc
    }
}

// Planning selects an identity without acquiring a Line lock. All descriptor
// and mapping checks precede mutation in commit_resolution under the caller's
// continuous registry and Line exclusion.
struct IrqResolution {
    virq: Virq,
    offered: Option<IrqDesc>,
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

    #[cfg(any(target_arch = "x86_64", unittest))]
    fn remove_mapping(&mut self, hwirq: Hwirq) -> bool {
        self.hwirq_to_virq.remove(&hwirq).is_some()
    }

    #[cfg(any(target_arch = "x86_64", unittest))]
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
}

/// Hardware operations and the disable depth to publish after they complete.
/// Preparing or discarding this plan does not change runtime state.
#[must_use = "preparing a plan has no effect; apply and commit it to change runtime state"]
#[derive(Clone, Copy, Debug)]
pub(super) struct IrqPlatformPlan {
    cpu: Option<usize>,
    pub(super) desc: IrqDesc,
    pub(super) configure: bool,
    pub(super) enable: Option<bool>,
    pub(super) flow_unmask: Option<fn(IrqDesc)>,
    disable_depth: usize,
}

impl IrqPlatformPlan {
    const fn none(desc: IrqDesc, disable_depth: usize) -> Self {
        Self {
            cpu: None,
            desc,
            configure: false,
            enable: None,
            flow_unmask: None,
            disable_depth,
        }
    }
}

pub(super) struct IrqDescRuntimeState {
    actions: [Option<IrqAction>; MAX_IRQ_ACTIONS],
    action_count: usize,
    generation: u64,
    configured_generation: Option<u64>,
    is_configuration_failed: bool,
    is_enabled: bool,
    disable_depth: usize,
    in_flight: usize,
    in_flight_zero: Arc<Completion>,
    teardown_depth: usize,
    is_msi: bool,
    oneshot_mask_pending: bool,
    next_shared_action_token: usize,
    unhandled: UnhandledIrqState,
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
            is_configuration_failed: false,
            is_enabled: false,
            disable_depth: 0,
            in_flight: 0,
            in_flight_zero,
            teardown_depth: 0,
            is_msi: desc.flags.contains(IrqFlags::MSI),
            oneshot_mask_pending: false,
            next_shared_action_token: 1,
            unhandled: UnhandledIrqState::new(),
        }
    }

    fn remember_desc(&mut self, desc: IrqDesc) {
        self.is_msi = desc.flags.contains(IrqFlags::MSI);
    }

    fn bump_generation(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    fn prepare_auto_enable(&self, desc: IrqDesc) -> IrqPlatformPlan {
        self.prepare_enable_at_depth_zero(desc, false)
    }

    fn prepare_register_disabled(&self, desc: IrqDesc) -> IrqPlatformPlan {
        IrqPlatformPlan::none(desc, self.disable_depth.max(1))
    }

    fn prepare_enable_irq(&self, desc: IrqDesc, force_platform_enable: bool) -> IrqPlatformPlan {
        let disable_depth = self.disable_depth.saturating_sub(1);
        if disable_depth > 0 {
            return IrqPlatformPlan::none(desc, disable_depth);
        }
        self.prepare_enable_at_depth_zero(desc, force_platform_enable)
    }

    fn prepare_enable_at_depth_zero(
        &self,
        desc: IrqDesc,
        force_platform_enable: bool,
    ) -> IrqPlatformPlan {
        IrqPlatformPlan {
            cpu: None,
            desc,
            configure: self.is_configuration_failed
                || self.configured_generation != Some(self.generation),
            enable: (force_platform_enable || !self.is_enabled || self.is_configuration_failed)
                .then_some(true),
            flow_unmask: None,
            disable_depth: 0,
        }
    }

    fn prepare_reconfigure_if_stale(&self, desc: IrqDesc) -> IrqPlatformPlan {
        IrqPlatformPlan {
            configure: self.is_configuration_failed
                || self.configured_generation != Some(self.generation),
            ..IrqPlatformPlan::none(desc, self.disable_depth)
        }
    }

    fn prepare_disable_irq_nosync(&self, desc: IrqDesc) -> IrqPlatformPlan {
        IrqPlatformPlan {
            enable: (self.disable_depth == 0 && self.is_enabled).then_some(false),
            ..IrqPlatformPlan::none(desc, self.disable_depth.saturating_add(1))
        }
    }

    fn prepare_disable_if_no_actions(&self, desc: IrqDesc) -> Option<IrqPlatformPlan> {
        // Old snapshots remain covered by drain; only action ownership decides
        // whether the final withdrawal must mask the source.
        if self.action_count != 0 {
            return None;
        }
        Some(IrqPlatformPlan {
            enable: self.is_enabled.then_some(false),
            ..IrqPlatformPlan::none(desc, self.disable_depth.max(1))
        })
    }

    fn can_install_regular_action(&self) -> bool {
        self.teardown_depth == 0 && self.action_count == 0
    }

    fn install_regular_action(&mut self, action: IrqAction) -> bool {
        if !self.can_install_regular_action() {
            return false;
        }
        self.actions[0] = Some(action);
        self.action_count = 1;
        true
    }

    fn take_regular_action(&mut self) -> Option<IrqAction> {
        if self.actions[0]
            .as_ref()
            .is_some_and(IrqAction::is_nmi_fallback)
        {
            return None;
        }
        self.take_single_action()
    }

    fn take_nmi_fallback(&mut self) -> Option<IrqAction> {
        if !self.actions[0]
            .as_ref()
            .is_some_and(IrqAction::is_nmi_fallback)
        {
            return None;
        }
        self.take_single_action()
    }

    fn take_single_action(&mut self) -> Option<IrqAction> {
        if self.action_count != 1 {
            return None;
        }
        // Shared removal leaves holes; a count of one does not imply slot zero
        // is occupied. Only regular/NMI installation guarantees that slot.
        if self.actions[0].as_ref()?.is_shared() {
            return None;
        }
        let action = self.actions[0].take()?;
        self.action_count = 0;
        self.oneshot_mask_pending = false;
        Some(action)
    }

    fn can_install_shared_action(&self) -> bool {
        self.teardown_depth == 0
            && self.action_count < MAX_IRQ_ACTIONS
            && self.next_shared_action_token.checked_add(1).is_some()
            && self.actions.iter().flatten().all(IrqAction::is_shared)
            && self.actions.iter().any(Option::is_none)
    }

    fn install_shared_action(
        &mut self,
        handler: crate::platform::Handler,
    ) -> Option<IrqActionToken> {
        if !self.can_install_shared_action() {
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

    fn finish_dispatch(&mut self) -> Option<DeferredWake> {
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

    #[cfg_attr(
        not(target_arch = "x86_64"),
        expect(
            clippy::allow_attributes,
            reason = "CG-024-lint-exceptions: MSI teardown queries are used only by the x86 MSI \
                      backend"
        )
    )]
    #[cfg_attr(
        not(target_arch = "x86_64"),
        allow(
            dead_code,
            reason = "CG-024-lint-exceptions: dead_code: MSI teardown queries are used only by \
                      the x86 MSI backend"
        )
    )]
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

/// Retained storage for one original IRQ line's mutable state.
///
/// The registry owns an `Arc`; retaining another owner preserves this object
/// after namespace removal, without redirecting it to a same-number successor.
/// Construct candidates outside short locks, including their runtime completion
/// and management mutex. Acquire management exclusion before global state or
/// Line state locks; the short-lock order remains global state then Line, never
/// Line then global state. Release the Line guard before registry removal.
/// Retention alone does not admit an operation: callers must revalidate original
/// Line membership and pair management admission under global state serialization.
pub(super) struct IrqLine {
    // Assigned before publication, independent of mutable controller metadata.
    virq: Once<Virq>,
    pub(super) management: kmutex::Mutex<()>,
    state: SpinNoIrq<IrqStateDesc>,
    replay_links: intrusive_collections::LinkedListAtomicLink,
}

impl IrqLine {
    pub(super) fn new(state: IrqStateDesc) -> Self {
        Self {
            virq: Once::new(),
            management: kmutex::Mutex::new(()),
            state: SpinNoIrq::new(state),
            replay_links: intrusive_collections::LinkedListAtomicLink::new(),
        }
    }

    pub(super) fn lock(&self) -> SpinNoIrqGuard<'_, IrqStateDesc> {
        self.state.lock()
    }

    /// Observes the assigned identity without locking, waiting or cloning an owner.
    /// None means publication has not assigned it; Some survives retirement and
    /// does not grant service admission or prove current directory membership.
    pub(super) fn logical_irq(&self) -> Option<Virq> {
        self.virq.get().copied()
    }

    #[cfg(unittest)]
    pub(super) fn is_state_locked_for_tests(&self) -> bool {
        self.state.is_locked()
    }
}

intrusive_adapter!(pub(super) ReplayAdapter = Arc<IrqLine>: IrqLine {
    replay_links => intrusive_collections::LinkedListAtomicLink
});

pub(super) struct IrqStateDesc {
    pub(super) desc: IrqDesc,
    #[cfg(any(target_arch = "x86_64", unittest))]
    msi_reservation: Option<MsiReservation>,
    runtime: IrqDescRuntimeState,
    is_managed: bool,
    // One-way storage lifecycle state, distinct from reversible action teardown.
    is_retired: bool,
    pub(super) flow: crate::IrqFlow,
    pub(super) is_running: bool,
    // Software service has no hardware claim keeping the gateway occupied.
    pub(super) is_replaying: bool,
    pub(super) is_pending: bool,
    pub(super) is_replay_queued: bool,
    local: [local::LocalControl; kbuild_config::NR_CPUS],
}

impl IrqStateDesc {
    pub(super) fn new(desc: IrqDesc) -> Self {
        Self {
            desc,
            #[cfg(any(target_arch = "x86_64", unittest))]
            msi_reservation: None,
            runtime: IrqDescRuntimeState::new(desc),
            is_managed: false,
            is_retired: false,
            flow: crate::IrqFlow::Legacy,
            is_running: false,
            is_replaying: false,
            is_pending: false,
            is_replay_queued: false,
            local: [local::LocalControl::new(); kbuild_config::NR_CPUS],
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
        !self.is_retired && self.runtime.install_regular_action(action)
    }

    pub(super) fn can_install_regular_action(&self) -> bool {
        !self.is_retired && self.runtime.can_install_regular_action()
    }

    pub(super) fn can_install_shared_action(&self) -> bool {
        !self.is_retired && self.runtime.can_install_shared_action()
    }

    pub(super) fn take_regular_action(&mut self) -> Option<IrqAction> {
        self.runtime.take_regular_action()
    }

    pub(super) fn take_nmi_fallback(&mut self) -> Option<IrqAction> {
        self.runtime.take_nmi_fallback()
    }

    pub(super) fn install_shared_action(
        &mut self,
        handler: crate::platform::Handler,
    ) -> Option<IrqActionToken> {
        if self.is_retired {
            return None;
        }
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
        IrqActionSnapshot { actions }
    }

    pub(super) fn prepare_auto_enable(&self) -> IrqPlatformPlan {
        if self.flow == crate::IrqFlow::PerCpu {
            let cpu = local::current_cpu();
            return self.local[cpu].enable(self.desc, self.runtime.generation, cpu, false, true);
        }
        let plan = self.runtime.prepare_auto_enable(self.desc);
        self.with_flow_unmask(plan)
    }

    pub(super) fn prepare_register_disabled(&self) -> IrqPlatformPlan {
        if self.flow == crate::IrqFlow::PerCpu {
            let cpu = local::current_cpu();
            return self.local[cpu].register_disabled(self.desc, cpu);
        }
        self.runtime.prepare_register_disabled(self.desc)
    }

    pub(super) fn prepare_enable_irq(&self) -> IrqPlatformPlan {
        if self.flow == crate::IrqFlow::PerCpu {
            let cpu = local::current_cpu();
            return self.local[cpu].enable(self.desc, self.runtime.generation, cpu, false, false);
        }
        let plan = self.runtime.prepare_enable_irq(self.desc, false);
        self.with_flow_unmask(plan)
    }

    pub(super) fn prepare_legacy_enable(&self) -> IrqPlatformPlan {
        if self.flow == crate::IrqFlow::PerCpu {
            let cpu = local::current_cpu();
            return self.local[cpu].enable(self.desc, self.runtime.generation, cpu, true, false);
        }
        let plan = self.runtime.prepare_enable_irq(self.desc, true);
        self.with_flow_unmask(plan)
    }

    fn with_flow_unmask(&self, mut plan: IrqPlatformPlan) -> IrqPlatformPlan {
        // Logical enable may already be set when a no-action arrival caused
        // flow suppression. Eligibility, not a new enable write, restores it.
        if plan.enable.unwrap_or(self.runtime.is_enabled)
            && plan.disable_depth == 0
            && self.has_actions()
            && !self.is_teardown_in_progress()
        {
            plan.flow_unmask = self.flow.unmask();
        }
        plan
    }

    pub(super) fn prepare_reconfigure_if_stale(&self) -> IrqPlatformPlan {
        if self.flow == crate::IrqFlow::PerCpu {
            let cpu = local::current_cpu();
            return self.local[cpu].reconfigure(self.desc, self.runtime.generation, cpu);
        }
        self.runtime.prepare_reconfigure_if_stale(self.desc)
    }

    pub(super) fn mark_configuration_failed(&mut self) {
        if self.flow == crate::IrqFlow::PerCpu {
            self.local[local::current_cpu()].has_failed_configuration = true;
            return;
        }
        self.runtime.is_configuration_failed = true;
    }

    pub(super) fn needs_configuration_restore(&self) -> bool {
        if self.flow == crate::IrqFlow::PerCpu {
            let control = &self.local[local::current_cpu()];
            return control.has_failed_configuration && control.is_enabled;
        }
        self.runtime.is_configuration_failed
            && self.runtime.is_enabled
            && self.runtime.disable_depth == 0
    }

    pub(super) fn prepare_disable_irq_nosync(&self) -> IrqPlatformPlan {
        if self.flow == crate::IrqFlow::PerCpu {
            let cpu = local::current_cpu();
            return self.local[cpu].disable(self.desc, cpu);
        }
        self.runtime.prepare_disable_irq_nosync(self.desc)
    }

    pub(super) fn prepare_disable_if_no_actions(&self) -> Option<IrqPlatformPlan> {
        if self.flow == crate::IrqFlow::PerCpu {
            return (!self.has_actions()).then(|| self.prepare_disable_irq_nosync());
        }
        self.runtime.prepare_disable_if_no_actions(self.desc)
    }

    /// Commits only after hardware apply returns, under the same Line guard
    /// used to prepare the plan. Descriptor generation, enabled state and disable
    /// depth must not change between prepare and commit. Action/teardown
    /// ownership changes are separate and are not rolled back.
    pub(super) fn commit_platform_plan(&mut self, plan: IrqPlatformPlan) {
        if let Some(cpu) = plan.cpu {
            debug_assert_eq!(cpu, local::current_cpu());
            self.local[cpu].commit(plan, self.runtime.generation);
            return;
        }
        if plan.configure {
            self.runtime.configured_generation = Some(self.runtime.generation);
            self.runtime.is_configuration_failed = false;
        }
        if let Some(enabled) = plan.enable {
            self.runtime.is_enabled = enabled;
        }
        self.runtime.disable_depth = plan.disable_depth;
    }

    pub(super) fn begin_dispatch(&mut self) {
        debug_assert!(!self.is_retired, "dispatch must validate Line lifetime");
        self.runtime.begin_dispatch();
    }

    pub(super) fn can_run_actions(&self) -> bool {
        !self.is_retired
            && !self.runtime.is_configuration_failed
            && self.runtime.is_enabled
            && self.runtime.disable_depth == 0
            && self.has_actions()
            && !self.is_teardown_in_progress()
    }

    pub(super) fn can_run_local_actions(&self) -> bool {
        !self.is_retired
            && self.local[local::current_cpu()].can_run()
            && self.has_actions()
            && !self.is_teardown_in_progress()
    }

    pub(super) fn has_enabled_local_cpu(&self) -> bool {
        self.flow == crate::IrqFlow::PerCpu && self.local.iter().any(|cpu| cpu.is_enabled)
    }

    pub(super) fn needs_replay(&self) -> bool {
        !matches!(self.flow, crate::IrqFlow::Legacy | crate::IrqFlow::PerCpu)
            && self.is_pending
            && !self.is_running
            && !self.is_replay_queued
            && self.can_run_actions()
    }

    pub(super) fn finish_dispatch(&mut self) -> Option<DeferredWake> {
        self.runtime.finish_dispatch()
    }

    pub(super) fn record_unhandled_dispatch(&mut self) -> Option<UnhandledIrqReport> {
        self.runtime.unhandled.record()
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

    #[cfg_attr(
        not(target_arch = "x86_64"),
        expect(
            clippy::allow_attributes,
            reason = "CG-024-lint-exceptions: MSI teardown queries are used only by the x86 MSI \
                      backend"
        )
    )]
    #[cfg_attr(
        not(target_arch = "x86_64"),
        allow(
            dead_code,
            reason = "CG-024-lint-exceptions: dead_code: MSI teardown queries are used only by \
                      the x86 MSI backend"
        )
    )]
    pub(super) fn is_msi(&self) -> bool {
        self.runtime.is_msi()
    }

    pub(super) fn is_unused(&self) -> bool {
        !self.is_managed && self.runtime.is_unused() && !self.has_enabled_local_cpu()
    }

    pub(super) fn is_retired(&self) -> bool {
        self.is_retired
    }

    /// Pins directory membership for an admitted management transaction.
    pub(super) fn begin_management(&mut self) {
        debug_assert!(!self.is_retired, "management must validate Line lifetime");
        debug_assert!(!self.is_managed, "management admission must be paired");
        self.is_managed = true;
    }

    /// Releases admission; descriptor disposal remains a separate operation.
    pub(super) fn end_management(&mut self) {
        debug_assert!(self.is_managed, "management completion requires admission");
        self.is_managed = false;
    }

    /// Ends an admitted transaction without changing directory membership.
    pub(super) fn finish_management(&mut self, has_removed_action: bool) {
        if has_removed_action {
            self.finish_teardown();
        }
        self.end_management();
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
    /// queries, and normal action dispatch.
    pub(super) descs: BTreeMap<Virq, Arc<IrqLine>>,
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
    pub(super) const fn new() -> Self {
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

    /// Publishes a private allocation as a new identity, never an existing binding.
    #[cfg(any(target_arch = "x86_64", unittest))]
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
        self.commit_resolution(resolution, line, &mut line.lock())
    }

    /// Resolves under the registry lock using a fresh candidate if absent.
    ///
    /// The caller retains `prepared` until outside short locks. An existing
    /// entry ignores it; a missing entry requires an unpublished candidate.
    pub(super) fn try_resolve_desc(
        &mut self,
        desc: IrqDesc,
        prepared: Option<&Arc<IrqLine>>,
    ) -> Result<ResolvedIrqDesc, IrqDescError> {
        let resolution = self.plan_resolution(IrqSpec::Desc(desc))?;
        self.resolve_planned(resolution, prepared)
    }

    fn plan_resolution(&mut self, spec: IrqSpec) -> Result<IrqResolution, IrqDescError> {
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
            .or(prepared)
            .expect("missing Line requires a prepared candidate")
            .clone();
        let mut entry = line.lock();
        self.commit_resolution(resolution, &line, &mut entry)
    }

    // The supplied entry is locked from this Line. Registry exclusion must
    // remain held from planning through commit, so identity cannot be replaced.
    fn commit_resolution(
        &mut self,
        resolution: IrqResolution,
        line: &Arc<IrqLine>,
        entry: &mut IrqStateDesc,
    ) -> Result<ResolvedIrqDesc, IrqDescError> {
        self.commit_resolution_with(resolution, line, entry, crate::domain::prepare_mapping)
    }

    fn commit_resolution_with(
        &mut self,
        resolution: IrqResolution,
        line: &Arc<IrqLine>,
        entry: &mut IrqStateDesc,
        prepare: impl FnOnce(
            IrqDomainId,
            alloc::vec::Vec<(Hwirq, Virq)>,
            Hwirq,
            Virq,
        ) -> Result<crate::domain::PreparedPublication, IrqDescError>,
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
            if !Arc::ptr_eq(current, line) {
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
        let flow = crate::IntrManagerIf::select_flow(stored_desc)?;
        // Flow operations and source identity must stay fixed while a handler
        // or queued service can still use this Line. Validate before mapping writes.
        if is_existing
            && (entry.has_actions() || entry.in_flight() != 0 || entry.has_enabled_local_cpu())
            && (entry.flow != flow
                || (!matches!(flow, crate::IrqFlow::Legacy) && entry.desc != stored_desc))
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
        let mut publication = None;
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
            let entries = if domain == crate::desc::MSI_DOMAIN {
                alloc::vec::Vec::new()
            } else {
                self.domain_mapping_entries(domain)
                    .chain(core::iter::once((hwirq, virq)))
                    .collect()
            };
            publication = Some(prepare(domain, entries, hwirq, virq)?);
        }
        // All recoverable validation precedes identity assignment. Registry and
        // Line exclusion serialize this non-allocating, non-reentrant initializer;
        // readers use only Once::get, never an initialization/wait operation.
        line.virq.call_once(|| virq);
        if let Some((domain, hwirq, virq)) = mapping_to_insert {
            let inserted = self.domain_state_mut(domain).insert_mapping(hwirq, virq);
            debug_assert!(inserted, "domain mapping insertion was prevalidated");
            if inserted {
                let old_mapping = self.virq_to_mapping.insert(virq, (domain, hwirq));
                debug_assert_eq!(old_mapping, None, "virq mapping insertion was prevalidated");
            }
        }
        entry.flow = flow;
        if is_existing {
            entry.update_desc(stored_desc);
        } else {
            // First publication preserves the candidate's initial generation
            // and preallocated completion/management storage.
            entry.desc = stored_desc;
            entry.runtime.remember_desc(stored_desc);
            self.descs.insert(virq, line.clone());
        }
        Ok(ResolvedIrqDesc::new(stored_desc, publication))
    }

    pub(super) fn try_resolve_spec(
        &mut self,
        spec: IrqSpec,
        prepared: Option<&Arc<IrqLine>>,
    ) -> Result<ResolvedIrqDesc, IrqDescError> {
        match spec {
            IrqSpec::PlainVirq(_) => {
                let resolution = self.plan_resolution(spec)?;
                self.resolve_planned(resolution, prepared)
            }
            IrqSpec::Desc(desc) => self.try_resolve_desc(desc, prepared),
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
        self.descs.get(&virq).map(|line| line.lock().desc)
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

    /// Explicit cleanup of unused, synthetic unbound test Lines.
    #[cfg(unittest)]
    pub(super) fn remove_if_unused(&mut self, virq: Virq) -> Option<IrqDesc> {
        self.remove_if_unused_inner(virq, false)
    }

    #[cfg(any(target_arch = "x86_64", unittest))]
    fn remove_if_unused_inner(&mut self, virq: Virq, remove_msi: bool) -> Option<IrqDesc> {
        let desc = {
            let mut entry = self.descs.get(&virq)?.lock();
            // Fixture cleanup must not dispose a Domain or MSI owner. The
            // production caller reaches this helper only for explicit MSI free.
            if !remove_msi
                && (entry.desc.domain.is_some() || entry.desc.flags.contains(IrqFlags::MSI))
            {
                return None;
            }
            if !entry.is_unused() {
                return None;
            }
            // Close old-reference admission before removing either index.
            // The registry lock still serializes all publication and disposal.
            entry.is_retired = true;
            entry.desc
        };
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

    /// Removes a software-only fixture, including any unexpectedly published mapping.
    /// The test owns the Line exclusively; no real source or dispatch may use it.
    #[cfg(unittest)]
    pub(super) fn remove_test_line(&mut self, virq: Virq) -> Option<Arc<IrqLine>> {
        let publication = if let Some((domain, hwirq)) = self.mapping_for_virq(virq) {
            if domain == crate::desc::MSI_DOMAIN {
                if !crate::domain::remove_msi(hwirq, || true) {
                    return None;
                }
                None
            } else {
                let entries: alloc::vec::Vec<_> = self
                    .domain_mapping_entries(domain)
                    .filter(|&(hardware, _)| hardware != hwirq)
                    .collect();
                Some(crate::domain::prepare_snapshot(domain, &entries).ok()?)
            }
        } else {
            None
        };
        self.descs.get(&virq)?.lock().is_retired = true;
        if let Some((domain, hwirq)) = self.virq_to_mapping.remove(&virq) {
            if let Some(domain_state) = self.domain_states.get_mut(&domain) {
                domain_state.remove_mapping(hwirq);
                if domain_state.is_empty() {
                    self.domain_states.remove(&domain);
                }
            }
            if let Some(publication) = publication {
                publication.publish();
            }
        }
        // Return ownership so final handler destruction occurs outside the lock.
        self.descs.remove(&virq)
    }

    #[cfg(any(target_arch = "x86_64", unittest))]
    pub(super) fn remove_msi_if_unused(&mut self, virq: Virq) -> Option<Hwirq> {
        self.remove_msi_with(virq, |_| true)
    }

    #[cfg(any(target_arch = "x86_64", unittest))]
    fn remove_msi_with(
        &mut self,
        virq: Virq,
        release: impl FnOnce(Hwirq) -> bool,
    ) -> Option<Hwirq> {
        let desc = {
            let entry = self.descs.get(&virq)?.lock();
            if !entry.is_msi() || !entry.is_unused() {
                return None;
            }
            entry.desc
        };
        let publication = if let Some(domain) = desc.domain
            && domain != crate::desc::MSI_DOMAIN
        {
            let entries: alloc::vec::Vec<_> = self
                .domain_mapping_entries(domain)
                .filter(|&(hwirq, _)| hwirq != desc.hwirq)
                .collect();
            Some(crate::domain::prepare_snapshot(domain, &entries).ok()?)
        } else {
            None
        };
        let released = if desc.domain == Some(crate::desc::MSI_DOMAIN) {
            crate::domain::remove_msi(desc.hwirq, || release(desc.hwirq))
        } else {
            release(desc.hwirq)
        };
        if !released {
            return None;
        }
        self.remove_if_unused_inner(virq, true)?;
        if let Some(publication) = publication {
            publication.publish();
        }
        Some(desc.hwirq)
    }
}

pub(super) fn try_resolve_and_publish(
    state: &mut IrqState,
    spec: IrqSpec,
    prepared: Option<&Arc<IrqLine>>,
) -> Result<IrqDesc, IrqDescError> {
    validate_resolution_domain(spec)?;
    let resolved = state.try_resolve_spec(spec, prepared)?;
    Ok(publish_resolution(resolved))
}

/// Resolves while the caller keeps the selected Line locked through its update.
///
/// The caller holds registry exclusion, selects the existing Line or an
/// unpublished candidate, and passes that Line's locked state. This function
/// never reacquires a Line lock. Rejected validation does not change metadata
/// or mappings; allocating a fresh dynamic identity may advance the allocator.
pub(super) fn try_resolve_and_publish_locked(
    state: &mut IrqState,
    spec: IrqSpec,
    line: &Arc<IrqLine>,
    entry: &mut IrqStateDesc,
) -> Result<IrqDesc, IrqDescError> {
    validate_resolution_domain(spec)?;
    let resolution = state.plan_resolution(spec)?;
    let resolved = state.commit_resolution(resolution, line, entry)?;
    Ok(publish_resolution(resolved))
}

fn validate_resolution_domain(spec: IrqSpec) -> Result<(), IrqDescError> {
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

fn publish_resolution(resolved: ResolvedIrqDesc) -> IrqDesc {
    if let Some(publication) = resolved.publication {
        publication.publish();
    }
    resolved.desc
}

#[cfg(target_arch = "x86_64")]
#[expect(
    clippy::expect_used,
    reason = "CG-021-no-panic-on-reachable-failure: a rejected private candidate retains its \
              reservation; successful resolution always assigns a logical IRQ"
)]
pub(crate) fn publish_msi_reservation(
    desc: IrqDesc,
    reservation: MsiReservation,
) -> Result<Virq, (IrqDescError, MsiReservation)> {
    let mut entry = IrqStateDesc::new(desc);
    entry.msi_reservation = Some(reservation);
    let line = Arc::new(IrqLine::new(entry));
    let result = {
        let mut state = IRQ_STATE.lock();
        state
            .insert_msi_reservation(desc, &line)
            .map(publish_resolution)
    };
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

#[cfg(target_arch = "x86_64")]
pub(crate) fn free_msi_if_unused(
    virq: Virq,
    free_backend_vector_fn: impl FnOnce(MsiReservation),
) -> bool {
    let removed = {
        let mut state = IRQ_STATE.lock();
        release_unused_msi(&mut state, virq, |_, reservation| {
            free_backend_vector_fn(reservation);
        })
    };
    // Final Line destruction is outside registry and backend allocator locks.
    removed.is_some()
}

// The production caller holds IRQ_STATE across validation, mapping withdrawal
// and backend release. Retain the Line beyond this lock, even though admission
// is already empty. Tests observe the withdrawn registry through the callback.
#[cfg(any(target_arch = "x86_64", unittest))]
#[expect(
    clippy::expect_used,
    reason = "CG-021-no-panic-on-reachable-failure: registry exclusion spans reservation \
              validation and withdrawal; no other writer can consume that owner"
)]
fn release_unused_msi(
    state: &mut IrqState,
    virq: Virq,
    free_backend_vector_fn: impl FnOnce(&IrqState, MsiReservation),
) -> Option<Arc<IrqLine>> {
    let line = state.descs.get(&virq)?.clone();
    {
        let entry = line.lock();
        // Legacy force-enable can open a Line without installing an action.
        // Empty admission alone is therefore not permission to return its vector.
        if !entry.is_msi() || !entry.is_unused() || entry.runtime.is_enabled {
            return None;
        }
        if entry.msi_reservation.as_ref()?.hwirq() != entry.desc.hwirq {
            return None;
        }
    }
    state.remove_msi_if_unused(virq)?;
    let reservation = line
        .lock()
        .msi_reservation
        .take()
        .expect("withdrawn MSI Line retains its validated reservation");
    free_backend_vector_fn(state, reservation);
    Some(line)
}

unittest::path_mod_test!(tests_line_admission, "../../ut-tests/line_admission.rs");
unittest::path_mod_test!(tests_msi_reservation, "../../ut-tests/msi_reservation.rs");
unittest::path_mod_test!(tests_mapping_lifetime, "../../ut-tests/mapping_lifetime.rs");

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IRQ descriptor and action state.

use alloc::sync::Arc;
use core::array;

use intrusive_collections::intrusive_adapter;

#[cfg(any(target_arch = "x86_64", unittest))]
use crate::backend::msi::MsiReservation;
#[path = "state_local.rs"]
mod local;

use kcompletion::{Completion, DeferredWake};
use klazy::Once;
use kspin::{SpinNoIrq, SpinNoIrqGuard, static_lock};

#[cfg(any(target_arch = "x86_64", unittest))]
use super::index::{PreparedWithdrawal, WithdrawnIndexes};
use super::{
    index::{LineIndex, PreparedPublication},
    unhandled::{UnhandledIrqReport, UnhandledIrqState},
};
use crate::{
    Hwirq, IrqDesc, IrqDescError, IrqDomainId, IrqFlags, IrqSpec, Virq,
    action::{IrqAction, IrqActionToken},
};

static_lock! {
    /// IRQ control-plane aggregate.
    ///
    /// All descriptor, handler, and `(domain, hwirq) -> virq` namespace changes go
    /// through this lock. Hardware dispatch retains a Line through the RCU index
    /// without this lock, then takes only the Line lock to obtain admission.
    pub(super) static IRQ_STATE: SpinNoIrq<IrqState> = SpinNoIrq::new(IrqState::global());
}
pub const DYNAMIC_VIRQ_BASE: Virq = 4096;
pub(super) const MAX_IRQ_ACTIONS: usize = 4;

/// Result of resolving one caller-provided IRQ descriptor.
///
/// Both prepared index insertions have completed before this result is returned.
pub(super) struct ResolvedIrqDesc {
    desc: IrqDesc,
}

impl ResolvedIrqDesc {
    const fn new(desc: IrqDesc) -> Self {
        Self { desc }
    }

    pub(super) const fn into_desc(self) -> IrqDesc {
        self.desc
    }
}

// Planning selects an identity without acquiring a Line lock. All descriptor
// and mapping checks precede mutation in commit_resolution under the caller's
// continuous registry and Line exclusion.
pub(super) struct IrqResolution {
    pub(super) virq: Virq,
    pub(super) offered: Option<IrqDesc>,
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
    // NMI ownership lives in IrqLine::nmi_action, not in this fanout array.
    has_nmi_action: bool,
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
            has_nmi_action: false,
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
        if self.has_nmi_action {
            return None;
        }
        self.take_single_action()
    }

    fn take_single_action(&mut self) -> Option<IrqAction> {
        if self.action_count != 1 {
            return None;
        }
        // Shared removal leaves holes; a count of one does not imply slot zero
        // is occupied. Only regular installation guarantees that slot.
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
            && !self.has_nmi_action
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
pub(crate) struct IrqLine {
    // Assigned before publication, independent of mutable controller metadata.
    virq: Once<Virq>,
    pub(super) management: kmutex::Mutex<()>,
    pub(super) nmi_action: krcu::RcuSlot<crate::platform::Handler>,
    state: SpinNoIrq<IrqStateDesc>,
    replay_links: intrusive_collections::LinkedListAtomicLink,
}

impl IrqLine {
    pub(super) fn new(state: IrqStateDesc) -> Self {
        Self {
            virq: Once::new(),
            management: kmutex::Mutex::new(()),
            nmi_action: krcu::RcuSlot::new(),
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
    pub(crate) fn logical_irq(&self) -> Option<Virq> {
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
    // Allocation-time storage, consumed only after successful release validation.
    #[cfg(any(target_arch = "x86_64", unittest))]
    msi_withdrawal: Option<PreparedWithdrawal>,
    runtime: IrqDescRuntimeState,
    is_managed: bool,
    // One-way storage lifecycle state, distinct from reversible action teardown.
    is_retired: bool,
    // Unlike transient teardown, a failed NMI close keeps enable admission shut.
    is_nmi_closing: bool,
    has_nmi_shutdown: bool,
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
            #[cfg(any(target_arch = "x86_64", unittest))]
            msi_withdrawal: None,
            runtime: IrqDescRuntimeState::new(desc),
            is_managed: false,
            is_retired: false,
            is_nmi_closing: false,
            has_nmi_shutdown: false,
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
        !self.is_retired && !self.has_nmi_shutdown && self.runtime.install_regular_action(action)
    }

    pub(super) fn can_install_regular_action(&self) -> bool {
        !self.is_retired && !self.has_nmi_shutdown && self.runtime.can_install_regular_action()
    }

    pub(super) fn can_install_shared_action(&self) -> bool {
        !self.is_retired && !self.has_nmi_shutdown && self.runtime.can_install_shared_action()
    }

    pub(super) fn take_regular_action(&mut self) -> Option<IrqAction> {
        self.runtime.take_regular_action()
    }

    /// Called under Line exclusion after the sole NMI owner is published.
    pub(super) fn install_nmi_action(&mut self) {
        debug_assert!(self.can_install_regular_action());
        self.runtime.has_nmi_action = true;
        self.runtime.action_count = 1;
    }

    /// Admission remains closed until withdrawal's grace period has completed.
    pub(super) fn finish_nmi_retirement(&mut self) {
        debug_assert!(self.has_nmi_action() && self.is_nmi_closing);
        self.runtime.has_nmi_action = false;
        self.runtime.action_count = 0;
        self.is_nmi_closing = self.has_nmi_shutdown;
    }

    pub(super) fn begin_nmi_close(&mut self) {
        self.is_nmi_closing = true;
    }

    pub(super) fn is_nmi_closing(&self) -> bool {
        self.is_nmi_closing
    }

    pub(super) fn has_nmi_shutdown(&self) -> bool {
        self.has_nmi_shutdown
    }

    pub(super) fn claim_nmi_shutdown(&mut self) -> [bool; kbuild_config::NR_CPUS] {
        debug_assert!(!self.has_nmi_shutdown && self.has_nmi_action());
        self.begin_nmi_close();
        self.has_nmi_shutdown = true;
        core::array::from_fn(|cpu| {
            self.local[cpu].is_enabled || self.local[cpu].has_nmi_preparation()
        })
    }

    pub(super) fn release_nmi_shutdown(&mut self) {
        debug_assert!(self.has_nmi_shutdown);
        self.has_nmi_shutdown = false;
        if !self.has_nmi_action() {
            self.is_nmi_closing = false;
        }
    }

    pub(super) fn prepare_local_nmi(
        &mut self,
        chip: &'static crate::PerCpuNmiChip,
    ) -> Result<(), crate::IrqDescError> {
        self.local[local::current_cpu()].prepare_nmi(self.desc, chip)
    }

    pub(super) fn teardown_local_nmi(&mut self) -> Result<(), crate::IrqDescError> {
        self.local[local::current_cpu()].teardown_nmi(self.desc)
    }

    pub(super) fn has_local_nmi_preparation(&self) -> bool {
        self.local
            .iter()
            .any(local::LocalControl::has_nmi_preparation)
    }

    pub(super) fn has_failed_local_nmi_preparation(&self) -> bool {
        self.local[local::current_cpu()].has_failed_nmi_preparation()
    }

    pub(super) fn has_nmi_action(&self) -> bool {
        self.runtime.has_nmi_action
    }

    pub(super) fn install_shared_action(
        &mut self,
        handler: crate::platform::Handler,
    ) -> Option<IrqActionToken> {
        if self.is_retired || self.has_nmi_shutdown {
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
        !self.is_managed
            && !self.has_nmi_shutdown
            && self.runtime.is_unused()
            && !self.has_enabled_local_cpu()
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
    pub(super) descs: LineIndex,
    /// Per-domain control-plane state.
    ///
    /// Each Domain maps hardware numbers directly to the same retained Lines.
    /// Dispatch reads the shared XArray through RCU without registry exclusion.
    domain_states: [LineIndex; crate::domain::DOMAIN_COUNT],
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
            descs: LineIndex::shared(&super::index::DIRECTORY),
            domain_states: [
                LineIndex::shared(&crate::domain::GIC_IRQ_DOMAIN.lines),
                LineIndex::shared(&crate::domain::PLIC_IRQ_DOMAIN.lines),
                LineIndex::shared(&crate::domain::IO_APIC_IRQ_DOMAIN.lines),
                LineIndex::shared(&crate::domain::MSI_IRQ_DOMAIN.lines),
                #[cfg(unittest)]
                LineIndex::shared(&crate::domain::TEST_IRQ_DOMAIN.lines),
                #[cfg(unittest)]
                LineIndex::shared(&crate::domain::SECOND_TEST_IRQ_DOMAIN.lines),
            ],
            next_virq: DYNAMIC_VIRQ_BASE,
            #[cfg(unittest)]
            index_observer: None,
        }
    }

    #[cfg(unittest)]
    pub(super) const fn new() -> Self {
        Self {
            descs: LineIndex::new(),
            domain_states: [const { LineIndex::new() }; crate::domain::DOMAIN_COUNT],
            next_virq: DYNAMIC_VIRQ_BASE,
            index_observer: None,
        }
    }

    pub(super) fn alloc_virq(&mut self) -> Result<Virq, IrqDescError> {
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
    pub(super) fn try_resolve_desc(
        &mut self,
        desc: IrqDesc,
        prepared: Option<&Arc<IrqLine>>,
    ) -> Result<ResolvedIrqDesc, IrqDescError> {
        let resolution = self.plan_resolution(IrqSpec::Desc(desc))?;
        self.resolve_planned(resolution, prepared)
    }

    pub(super) fn plan_resolution(&mut self, spec: IrqSpec) -> Result<IrqResolution, IrqDescError> {
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
    pub(super) fn commit_resolution(
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
        entry.flow = flow;
        if is_existing {
            entry.update_desc(stored_desc);
        } else {
            // First publication preserves the candidate's initial generation.
            entry.desc = stored_desc;
            entry.runtime.remember_desc(stored_desc);
        }
        if let Some((domain, ..)) = mapping_to_insert {
            #[expect(
                clippy::expect_used,
                reason = "CG-021-no-panic-on-reachable-failure: both publication capacities \
                          validated before either insertion"
            )]
            self.domain_states[crate::domain::slot(domain).expect("validated domain")].publish(
                line.clone(),
                prepared.domain.take().expect("prepared domain insertion"),
            );
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

    pub(super) fn lookup_virq(&self, spec: IrqSpec) -> Option<Virq> {
        match spec {
            IrqSpec::PlainVirq(virq) => Some(virq),
            IrqSpec::Desc(desc) => desc.logical_irq().or_else(|| {
                desc.domain
                    .and_then(|domain| self.domain_virq(domain, desc.hwirq))
            }),
        }
    }

    pub(super) fn mapping_to_prepare(&self, spec: IrqSpec) -> Option<(IrqDomainId, Hwirq)> {
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
    pub(super) fn lookup_dispatch_line(
        &self,
        source: crate::IrqRef,
    ) -> Option<(Virq, Option<Arc<IrqLine>>)> {
        let virq = match source {
            crate::IrqRef::Virq(virq) => virq,
            crate::IrqRef::Domain(domain, hwirq) => {
                if let Some(line) = self
                    .domain_states
                    .get(crate::domain::slot(domain)?)
                    .and_then(|state| state.get(&hwirq))
                {
                    return Some((line.logical_irq()?, Some(line)));
                }
                crate::domain::domain(domain)?.resolve_unmapped(hwirq)?
            }
        };
        Some((virq, self.descs.get(&virq)))
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
        self.domain_states[crate::domain::slot(domain)?]
            .get(&hwirq)?
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
    pub(super) fn remove_if_unused(&mut self, virq: Virq) -> Option<IrqDesc> {
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
    pub(super) fn remove_test_line(&mut self, virq: Virq) -> Option<Arc<IrqLine>> {
        let line = self.descs.get(&virq)?;
        let desc = {
            let mut entry = line.lock();
            entry.is_retired = true;
            entry.desc
        };
        if let Some(slot) = desc.domain.and_then(crate::domain::slot) {
            self.domain_states[slot].remove(&desc.hwirq);
        }
        self.descs.remove(&virq)
    }

    #[cfg(unittest)]
    pub(super) fn remove_msi_if_unused(&mut self, virq: Virq) -> Option<Hwirq> {
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
    pub(super) unsafe fn withdraw_msi(
        &mut self,
        virq: Virq,
    ) -> Option<(Arc<IrqLine>, MsiReservation, WithdrawnIndexes)> {
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
        let desc = entry.desc;
        let reservation = entry.msi_reservation.take()?;
        // SAFETY: caller preflight and registry exclusion span validation and
        // both withdrawals; every pending record is returned for unlocked submit.
        let domain = if let Some(slot) = desc.domain.and_then(crate::domain::slot) {
            // SAFETY: the caller's preflight and registry exclusion cover this
            // withdrawal; its pending record is transferred to the caller.
            unsafe { self.domain_states[slot].withdraw(desc.hwirq, prepared.domain) }
        } else {
            None
        };
        #[cfg(unittest)]
        self.observe_index_transition(&line, &entry);
        // SAFETY: caller preflight still spans this second withdrawal, and its
        // pending retirement is returned for submission outside exclusion.
        let logical = unsafe { self.descs.withdraw(virq, prepared.logical) };
        drop(entry);
        Some((line, reservation, WithdrawnIndexes { logical, domain }))
    }
}

/// Resolves while the caller keeps the selected Line locked through its update.
///
/// The caller holds registry exclusion, selects the existing Line or an
/// unpublished candidate, and passes that Line's locked state. This function
/// never reacquires a Line lock. Rejected validation does not change metadata
/// or mappings; allocating a fresh dynamic identity may advance the allocator.
pub(super) fn resolve_prepared_locked(
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

pub(super) fn validate_resolution_domain(spec: IrqSpec) -> Result<(), IrqDescError> {
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
pub(super) fn try_resolve_and_publish_locked(
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

#[cfg(target_arch = "x86_64")]
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
    let Some((line, reservation, pending)) = removed else {
        return false;
    };
    pending.retire();
    free_backend_vector_fn(reservation);
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
    let (line, reservation, pending) = unsafe { state.withdraw_msi(virq) }?;
    pending.retire();
    free_backend_vector_fn(state, reservation);
    Some(line)
}

unittest::path_mod_test!(tests_line_admission, "../../ut-tests/line_admission.rs");
unittest::path_mod_test!(tests_msi_reservation, "../../ut-tests/msi_reservation.rs");

#[cfg(unittest)]
pub use tests_msi_reservation::RcuIndexProbe;
unittest::path_mod_test!(tests_mapping_lifetime, "../../ut-tests/mapping_lifetime.rs");
unittest::path_mod_test!(
    tests_index_transactions,
    "../../ut-tests/index_transactions.rs"
);

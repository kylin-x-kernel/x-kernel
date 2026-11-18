// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Single-Line descriptor, action and execution-admission state.
//!
//! A retained `IrqLine` preserves one identity across namespace removal; its
//! short state lock protects action membership, flow binding and local control.
//! The management mutex serializes sleepable membership/lifecycle operations
//! before registry and Line locks. Retaining storage alone grants no admission.
//! Serialized ordinary flows use running ownership; NMI has a separate owner.
//! PerCpu reclamation relies on caller-provided dispatch/completion quiescence,
//! not on local enabled bits or ordinary running ownership.
//!
//! Global namespace, mapping and MSI transactions live in the private
//! `registry` child. The explicit reexports below preserve the internal facade
//! without exposing Line fields or moving controller/source policy here.

use alloc::sync::Arc;

use intrusive_collections::{LinkedList, intrusive_adapter};

#[cfg(any(target_arch = "x86_64", unittest))]
use crate::backend::msi::MsiReservation;
#[path = "state_local.rs"]
mod local;

use kcompletion::{Completion, DeferredWake};
use kcpu_id_map::{KCpuMask, KCpuMaskExt, LogicalCpuId};
use klazy::Once;
use kspin::{SpinNoIrq, SpinNoIrqGuard};

#[cfg(any(target_arch = "x86_64", unittest))]
use super::index::PreparedWithdrawal;
use super::{
    action_set::{ActionSet, RetiredAction},
    retirement::{RetirementAdapter, RetirementSlot},
    unhandled::{UnhandledIrqReport, UnhandledIrqState},
};
#[cfg(unittest)]
use crate::action::IrqAction;
use crate::{
    IrqDesc, IrqDescError, IrqFlags, Virq,
    action::{IrqActionToken, PreparedAction},
};

// Keep existing internal state imports stable while the registry owns namespace
// transactions. Child access avoids exposing Line internals to sibling modules.
mod registry;
pub use registry::DYNAMIC_VIRQ_BASE;
#[cfg(unittest)]
pub use registry::RcuIndexProbe;
#[cfg(any(target_arch = "x86_64", unittest))]
pub(crate) use registry::free_msi_if_unused;
#[cfg(any(target_arch = "x86_64", unittest))]
pub(crate) use registry::publish_msi_reservation;
pub(super) use registry::{
    IRQ_STATE, IrqState, resolve_prepared_locked, validate_resolution_domain,
};

pub(super) use super::action_set::IrqActionSnapshot;

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

/// Line-locked action membership, ordinary source control and drain notification.
/// The enclosing descriptor pairs this bookkeeping with flow admission. PerCpu
/// enable/configuration lives in LocalControl; NMI handler storage lives in
/// IrqLine::nmi_action. Neither the completion nor enabled bits prove source stop.
pub(super) struct IrqDescRuntimeState {
    actions: ActionSet,
    // NMI ownership lives in IrqLine::nmi_action, not in the ordinary collection.
    has_nmi_action: bool,
    generation: u64,
    configured_generation: Option<u64>,
    is_configuration_failed: bool,
    is_enabled: bool,
    disable_depth: usize,
    execution_idle: Arc<Completion>,
    teardown_depth: usize,
    is_msi: bool,
    oneshot_mask_pending: bool,
    unhandled: UnhandledIrqState,
}

impl IrqDescRuntimeState {
    fn new(desc: IrqDesc) -> Self {
        let execution_idle = Arc::new(Completion::new());
        execution_idle.complete_all();
        Self {
            actions: ActionSet::new(),
            has_nmi_action: false,
            generation: 0,
            configured_generation: None,
            is_configuration_failed: false,
            is_enabled: false,
            disable_depth: 0,
            execution_idle,
            teardown_depth: 0,
            is_msi: desc.flags.contains(IrqFlags::MSI),
            oneshot_mask_pending: false,
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
        // Compatibility bring-up can enable an actionless Line. Disabled
        // registration must close that delivery gate before committing depth.
        IrqPlatformPlan {
            enable: self.is_enabled.then_some(false),
            ..IrqPlatformPlan::none(desc, self.disable_depth.max(1))
        }
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
        // The removal path owns old-snapshot exclusion; only action ownership
        // decides whether the final withdrawal must mask the source.
        if self.has_actions() {
            return None;
        }
        Some(IrqPlatformPlan {
            enable: self.is_enabled.then_some(false),
            ..IrqPlatformPlan::none(desc, self.disable_depth.max(1))
        })
    }

    fn can_install_regular_action(&self) -> bool {
        self.teardown_depth == 0 && !self.has_actions() && self.actions.can_install_regular()
    }

    fn publish_regular_action(&mut self, prepared: &mut PreparedAction) -> bool {
        if !self.can_install_regular_action() {
            return false;
        }
        self.actions.publish_regular(prepared)
    }

    fn can_install_shared_action(&self) -> bool {
        self.teardown_depth == 0 && !self.has_nmi_action && self.actions.can_install_shared()
    }

    fn publish_shared_action(&mut self, prepared: &mut PreparedAction) -> Option<IrqActionToken> {
        if !self.can_install_shared_action() {
            return None;
        }
        self.actions.publish_shared(prepared)
    }

    fn withdraw_action(&mut self, token: Option<IrqActionToken>) -> Option<RetiredAction> {
        if self.has_nmi_action {
            return None;
        }
        let retired = self.actions.withdraw(token)?;
        if !self.has_actions() {
            self.oneshot_mask_pending = false;
        }
        Some(retired)
    }

    const fn has_actions(&self) -> bool {
        self.action_count() != 0
    }

    const fn action_count(&self) -> usize {
        self.actions.len() + self.has_nmi_action as usize
    }

    fn execution_idle_completion(&self) -> Arc<Completion> {
        self.execution_idle.clone()
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

    #[cfg_attr(
        not(target_arch = "x86_64"),
        expect(
            clippy::allow_attributes,
            reason = "CG-024-lint-exceptions: teardown queries are used only by the x86 MSI \
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
    const fn is_unused(&self) -> bool {
        !self.has_actions() && self.teardown_depth == 0
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
    pub(super) nmi_action: super::nmi::NmiAction,
    state: SpinNoIrq<IrqStateDesc>,
    replay_links: intrusive_collections::LinkedListAtomicLink,
}

impl IrqLine {
    pub(super) fn new(state: IrqStateDesc) -> Self {
        Self {
            virq: Once::new(),
            management: kmutex::Mutex::new(()),
            nmi_action: super::nmi::NmiAction::new(),
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

/// Mutable state for one Line, protected by its short IRQ-masking lock.
///
/// Flow/descriptor binding, action publication and execution admission are
/// validated together under this lock. Candidate, live and retired storage are
/// distinct from reversible disable/teardown state. Namespace ownership and
/// cross-index transactions belong to `registry`; this state does not prove
/// that an external interrupt producer has stopped.
pub(super) struct IrqStateDesc {
    pub(super) desc: IrqDesc,
    pub(super) affinity: crate::IrqAffinitySnapshot,
    #[cfg(unittest)]
    pub(super) test_affinity_chip: Option<&'static crate::IrqAffinityChip>,
    #[cfg(unittest)]
    pub(super) test_deferred_affinity: Option<&'static crate::IrqDeferredAffinityChip>,
    #[cfg(any(target_arch = "x86_64", unittest))]
    msi_reservation: Option<MsiReservation>,
    // Allocation-time storage, consumed only after successful release validation.
    #[cfg(any(target_arch = "x86_64", unittest))]
    msi_withdrawal: Option<PreparedWithdrawal>,
    runtime: IrqDescRuntimeState,
    is_managed: bool,
    // One-way storage lifecycle state, distinct from reversible action teardown.
    is_retired: bool,
    // Persistent fact for procfs visibility; never cleared on action removal.
    has_action_history: bool,
    retirement_slots: LinkedList<RetirementAdapter>,
    // Unlike transient teardown, a failed NMI close keeps enable admission shut.
    is_nmi_closing: bool,
    has_nmi_shutdown: bool,
    // A private candidate has no executable flow until resolution succeeds.
    pub(super) flow: Option<crate::IrqFlow>,
    #[cfg(unittest)]
    test_flow: Option<crate::IrqFlow>,
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
            affinity: crate::IrqAffinitySnapshot::default(),
            #[cfg(unittest)]
            test_affinity_chip: None,
            #[cfg(unittest)]
            test_deferred_affinity: None,
            #[cfg(any(target_arch = "x86_64", unittest))]
            msi_reservation: None,
            #[cfg(any(target_arch = "x86_64", unittest))]
            msi_withdrawal: None,
            runtime: IrqDescRuntimeState::new(desc),
            is_managed: false,
            is_retired: false,
            has_action_history: false,
            retirement_slots: LinkedList::new(RetirementAdapter::NEW),
            is_nmi_closing: false,
            has_nmi_shutdown: false,
            flow: None,
            #[cfg(unittest)]
            test_flow: None,
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
        if self.desc.hwirq != desc.hwirq
            || self.desc.domain != desc.domain
            || self.desc.controller != desc.controller
        {
            // Metadata rebinding cannot certify a previously established route
            // or clear a failed programming obligation.
            self.affinity.requested = None;
            self.affinity.effective = None;
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

    pub(super) fn publish_regular_action(&mut self, prepared: &mut PreparedAction) -> bool {
        let installed = self.flow.is_some()
            && !self.affinity.has_failed_programming
            && !self.is_retired
            && !self.has_nmi_shutdown
            && self.runtime.publish_regular_action(prepared);
        if installed {
            self.has_action_history = true;
        }
        installed
    }

    #[cfg(unittest)]
    pub(super) fn install_regular_action(&mut self, action: IrqAction) -> bool {
        let Ok(mut prepared) = PreparedAction::from_action(action) else {
            return false;
        };
        self.publish_regular_action(&mut prepared)
    }

    pub(super) fn can_install_regular_action(&self) -> bool {
        (!self.affinity.has_failed_programming || self.can_attempt_deferred_recovery())
            && !self.is_retired
            && !self.has_nmi_shutdown
            && self.runtime.can_install_regular_action()
    }

    /// A failed route may re-enter registration only for the masked retry that
    /// the activation path will resolve before publishing the action.
    fn can_attempt_deferred_recovery(&self) -> bool {
        self.affinity.is_pending && self.is_affinity_disabled()
    }

    pub(super) fn can_install_nmi_action(&self) -> bool {
        !self.affinity.has_failed_programming
            && !self.is_retired
            && !self.has_nmi_shutdown
            && self.runtime.teardown_depth == 0
            && !self.runtime.has_actions()
    }

    pub(super) fn can_install_shared_action(&self) -> bool {
        (!self.affinity.has_failed_programming || self.can_attempt_deferred_recovery())
            && !self.is_retired
            && !self.has_nmi_shutdown
            && self.runtime.can_install_shared_action()
    }

    pub(super) fn withdraw_action(
        &mut self,
        token: Option<IrqActionToken>,
    ) -> Option<RetiredAction> {
        if self.is_retired || self.has_nmi_shutdown {
            return None;
        }
        self.runtime.withdraw_action(token)
    }

    #[cfg(unittest)]
    pub(super) fn exhaust_action_sequences_for_tests(&mut self) {
        self.runtime.actions.exhaust_sequences();
    }

    /// Called under Line exclusion after the sole NMI owner is published.
    pub(super) fn install_nmi_action(&mut self) {
        debug_assert!(self.can_install_nmi_action());
        self.runtime.has_nmi_action = true;
        self.has_action_history = true;
    }

    /// Admission remains closed until local action publications are withdrawn.
    pub(super) fn finish_nmi_retirement(&mut self) {
        debug_assert!(self.has_nmi_action() && self.is_nmi_closing);
        self.runtime.has_nmi_action = false;
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

    pub(super) fn has_ready_local_nmi_preparation(&self) -> bool {
        let local = &self.local[local::current_cpu()];
        local.has_nmi_preparation() && !local.has_failed_nmi_preparation()
    }

    pub(super) fn has_nmi_action(&self) -> bool {
        self.runtime.has_nmi_action
    }

    pub(super) fn publish_shared_action(
        &mut self,
        prepared: &mut PreparedAction,
    ) -> Option<IrqActionToken> {
        if self.affinity.has_failed_programming
            || self.flow.is_none()
            || self.is_retired
            || self.has_nmi_shutdown
        {
            return None;
        }
        let token = self.runtime.publish_shared_action(prepared);
        if token.is_some() {
            self.has_action_history = true;
        }
        token
    }

    #[cfg(unittest)]
    pub(super) fn install_shared_action(
        &mut self,
        handler: crate::platform::Handler,
    ) -> Option<IrqActionToken> {
        let mut prepared = PreparedAction::shared(handler).ok()?;
        self.publish_shared_action(&mut prepared)
    }

    pub(super) fn has_actions(&self) -> bool {
        self.runtime.has_actions()
    }

    pub(super) fn action_count(&self) -> usize {
        self.runtime.action_count()
    }

    /// Returns whether any installed action refuses user-originated routing.
    ///
    /// Line exclusion must be held. The scan is control-plane only; dispatch
    /// never evaluates owner policy.
    pub(super) fn has_user_affinity_restriction(&self) -> bool {
        self.runtime.actions.has_user_affinity_restriction()
    }

    /// Captures membership under Line exclusion for an admitted traversal.
    ///
    /// # Safety
    /// Retain this Line until the snapshot's last access. Ordinary traversals
    /// must retain running ownership; PerCpu traversals require source-owned
    /// shutdown exclusion through their last access and original completion.
    /// Capture under the same Line lock as withdrawal. No removed node may be
    /// reclaimed before the corresponding execution lifetime ends.
    pub(super) unsafe fn snapshot_actions<'admission>(&self) -> IrqActionSnapshot<'admission> {
        // SAFETY: the caller supplies the original Line's execution lifetime;
        // its lock serializes this head capture with membership changes.
        unsafe { self.runtime.actions.snapshot() }
    }

    pub(super) fn prepare_auto_enable(&self) -> IrqPlatformPlan {
        if self.flow == Some(crate::IrqFlow::PerCpu) {
            let cpu = local::current_cpu();
            return self.local[cpu].enable(self.desc, self.runtime.generation, cpu);
        }
        let plan = self.runtime.prepare_auto_enable(self.desc);
        self.with_flow_unmask(plan)
    }

    pub(super) fn prepare_register_disabled(&self) -> IrqPlatformPlan {
        if self.flow == Some(crate::IrqFlow::PerCpu) {
            let cpu = local::current_cpu();
            return self.local[cpu].register_disabled(self.desc, cpu);
        }
        self.runtime.prepare_register_disabled(self.desc)
    }

    pub(super) fn prepare_enable_irq(&self) -> IrqPlatformPlan {
        if self.flow == Some(crate::IrqFlow::PerCpu) {
            let cpu = local::current_cpu();
            return self.local[cpu].enable(self.desc, self.runtime.generation, cpu);
        }
        let plan = self.runtime.prepare_enable_irq(self.desc, false);
        self.with_flow_unmask(plan)
    }

    pub(super) fn prepare_legacy_enable(&self) -> IrqPlatformPlan {
        if self.flow == Some(crate::IrqFlow::PerCpu) {
            let cpu = local::current_cpu();
            return self.local[cpu].enable(self.desc, self.runtime.generation, cpu);
        }
        let plan = self.runtime.prepare_enable_irq(self.desc, true);
        self.with_flow_unmask(plan)
    }

    fn with_flow_unmask(&self, mut plan: IrqPlatformPlan) -> IrqPlatformPlan {
        // Logical enable may already be set when a no-action arrival caused
        // flow suppression. Eligibility, not a new enable write, restores it.
        if plan.enable.unwrap_or(self.runtime.is_enabled)
            && !self.affinity.has_failed_programming
            && plan.disable_depth == 0
            && self.has_actions()
            && !self.is_teardown_in_progress()
        {
            plan.flow_unmask = self.flow.and_then(crate::IrqFlow::unmask);
        }
        plan
    }

    pub(super) fn prepare_reconfigure_if_stale(&self) -> IrqPlatformPlan {
        if self.flow == Some(crate::IrqFlow::PerCpu) {
            let cpu = local::current_cpu();
            return self.local[cpu].reconfigure(self.desc, self.runtime.generation, cpu);
        }
        self.runtime.prepare_reconfigure_if_stale(self.desc)
    }

    pub(super) fn mark_configuration_failed(&mut self) {
        if self.flow == Some(crate::IrqFlow::PerCpu) {
            self.local[local::current_cpu()].has_failed_configuration = true;
            return;
        }
        self.runtime.is_configuration_failed = true;
    }

    pub(super) fn needs_configuration_restore(&self) -> bool {
        if self.flow == Some(crate::IrqFlow::PerCpu) {
            let control = &self.local[local::current_cpu()];
            return control.has_failed_configuration && control.is_enabled;
        }
        self.runtime.is_configuration_failed
            && self.runtime.is_enabled
            && self.runtime.disable_depth == 0
    }

    pub(super) fn prepare_disable_irq_nosync(&self) -> IrqPlatformPlan {
        if self.flow == Some(crate::IrqFlow::PerCpu) {
            let cpu = local::current_cpu();
            return self.local[cpu].disable(self.desc, cpu);
        }
        self.runtime.prepare_disable_irq_nosync(self.desc)
    }

    pub(super) fn prepare_disable_if_no_actions(&self) -> Option<IrqPlatformPlan> {
        if self.flow == Some(crate::IrqFlow::PerCpu) {
            // PerCpu removal requires caller-completed shutdown on every CPU.
            // It must not perform another local gate operation on the freeing CPU.
            return None;
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

    pub(super) fn begin_running_dispatch(&mut self) {
        debug_assert!(!self.is_retired && !self.is_running);
        if !self.has_active_execution() {
            self.runtime.execution_idle.reinit();
        }
        self.is_running = true;
    }

    /// Caller has consumed every snapshot and completed the locked flow tail.
    pub(super) fn finish_running_dispatch(&mut self) -> Option<DeferredWake> {
        self.is_running = false;
        self.is_replaying = false;
        (!self.has_active_execution())
            .then(|| self.runtime.execution_idle.complete_all_defer_wake())
    }

    /// Ordinary flow activity only; PerCpu teardown requires external exclusion.
    pub(super) fn has_active_execution(&self) -> bool {
        self.is_running
    }

    pub(super) fn can_run_actions(&self) -> bool {
        self.flow.is_some()
            && !self.affinity.has_failed_programming
            && !self.is_retired
            && !self.runtime.is_configuration_failed
            && self.runtime.is_enabled
            && self.runtime.disable_depth == 0
            && self.has_actions()
            && !self.is_teardown_in_progress()
    }

    pub(super) fn can_run_local_actions(&self) -> bool {
        self.flow == Some(crate::IrqFlow::PerCpu)
            && !self.is_retired
            && self.local[local::current_cpu()].can_run()
            && self.has_actions()
            && !self.is_teardown_in_progress()
    }

    pub(super) fn has_enabled_local_cpu(&self) -> bool {
        self.flow == Some(crate::IrqFlow::PerCpu) && self.local.iter().any(|cpu| cpu.is_enabled)
    }

    fn is_binding_in_use(&self) -> bool {
        self.affinity.has_failed_programming
            || self.has_actions()
            || self.has_active_execution()
            || self.is_replay_queued
            || self.has_enabled_local_cpu()
    }

    fn select_binding_flow(&self, desc: IrqDesc) -> Result<crate::IrqFlow, IrqDescError> {
        #[cfg(unittest)]
        if let Some(flow) = self.test_flow {
            return Ok(flow);
        }
        crate::IntrManagerIf::select_flow(desc)
    }

    pub(super) fn needs_replay(&self) -> bool {
        matches!(self.flow, Some(flow) if flow != crate::IrqFlow::PerCpu)
            && self.is_pending
            && !self.is_running
            && !self.is_replay_queued
            && self.can_run_actions()
    }

    pub(super) fn record_unhandled_dispatch(&mut self) -> Option<UnhandledIrqReport> {
        self.runtime.unhandled.record()
    }

    pub(super) fn execution_idle_completion(&self) -> Arc<Completion> {
        self.runtime.execution_idle_completion()
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

    #[cfg_attr(
        not(target_arch = "x86_64"),
        expect(
            clippy::allow_attributes,
            reason = "CG-024-lint-exceptions: teardown queries are used only by the x86 MSI \
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
    pub(super) fn is_unused(&self) -> bool {
        !self.is_managed
            && !self.has_nmi_shutdown
            && !self.is_running
            && self.runtime.is_unused()
            && !self.has_enabled_local_cpu()
    }

    pub(super) fn is_retired(&self) -> bool {
        self.is_retired
    }

    pub(super) fn has_action_history(&self) -> bool {
        self.has_action_history
    }

    /// Computes the first-action policy without publishing it.
    ///
    /// The caller commits the returned mask only after action publication
    /// succeeds. Explicit requested values and source-managed defaults are
    /// retained/left unknown.
    pub(super) fn prepare_initial_affinity_policy(&self) -> Result<Option<KCpuMask>, IrqDescError> {
        self.prepare_initial_affinity_policy_with(kcpu_id_map::present_mask())
    }

    pub(super) fn prepare_initial_affinity_policy_with(
        &self,
        present: Option<KCpuMask>,
    ) -> Result<Option<KCpuMask>, IrqDescError> {
        if self.has_action_history || self.affinity.requested.is_some() {
            return Ok(None);
        }
        match self.desc.affinity {
            crate::IrqAffinity::Cpu(cpu) => {
                if cpu >= kbuild_config::NR_CPUS {
                    return Err(IrqDescError::InvalidCpu { cpu });
                }
                let mut mask = KCpuMask::new();
                mask.set_logical(LogicalCpuId::new(cpu), true);
                Ok(Some(mask))
            }
            crate::IrqAffinity::Any if self.is_ordinary_affinity_source() => {
                present.map(Some).ok_or(IrqDescError::NotReady)
            }
            crate::IrqAffinity::Any => Ok(None),
        }
    }

    pub(super) fn commit_initial_affinity_policy(&mut self, policy: Option<KCpuMask>) {
        if let Some(policy) = policy
            && self.affinity.requested.is_none()
        {
            self.affinity.requested = Some(policy);
        }
    }

    fn is_ordinary_affinity_source(&self) -> bool {
        self.flow.is_some()
            && self.flow != Some(crate::IrqFlow::PerCpu)
            && !self
                .desc
                .flags
                .intersects(IrqFlags::MSI | IrqFlags::PER_CPU)
            && !self.has_nmi_action()
            && !self.has_nmi_shutdown
    }

    pub(super) fn register_retirement(&mut self, slot: Arc<RetirementSlot>) -> bool {
        if self.is_retired {
            return false;
        }
        self.retirement_slots.push_back(slot);
        true
    }

    pub(super) fn remove_retirement(&mut self, slot: &Arc<RetirementSlot>) {
        if self.is_retired {
            return;
        }
        let target = Arc::as_ptr(slot);
        let mut cursor = self.retirement_slots.cursor_mut();
        cursor.move_next();
        while let Some(ptr) = cursor.get_ptr() {
            if ptr.as_ptr() == target.cast_mut() {
                cursor.remove();
                return;
            }
            cursor.move_next();
        }
    }

    #[cfg(any(target_arch = "x86_64", unittest))]
    pub(super) fn take_retirements(&mut self) -> LinkedList<RetirementAdapter> {
        core::mem::replace(
            &mut self.retirement_slots,
            LinkedList::new(RetirementAdapter::NEW),
        )
    }

    #[cfg(unittest)]
    pub(super) fn set_retired_for_tests(&mut self) {
        self.is_retired = true;
    }

    pub(super) fn affinity_chip(&self) -> Option<&'static crate::IrqAffinityChip> {
        #[cfg(unittest)]
        if let Some(chip) = self.test_affinity_chip {
            return Some(chip);
        }
        crate::IntrManagerIf::immediate_affinity(self.desc)
    }

    pub(super) fn deferred_affinity_chip(&self) -> Option<&'static crate::IrqDeferredAffinityChip> {
        #[cfg(unittest)]
        if let Some(chip) = self.test_deferred_affinity {
            return Some(chip);
        }
        crate::IntrManagerIf::deferred_affinity(self.desc)
    }

    /// Withdraws an accepted-but-not-effective request when the last action
    /// leaves. The confirmed route becomes the requested policy; a hardware
    /// failure stays latched and is not cleared by cancellation.
    pub(super) fn cancel_pending_affinity(&mut self) {
        if self.affinity.is_pending {
            self.affinity.requested = self.affinity.effective;
            self.affinity.is_pending = false;
        }
    }

    pub(super) fn is_affinity_disabled(&self) -> bool {
        self.runtime.disable_depth > 0 || !self.runtime.is_enabled
    }

    /// Trigger reconfiguration cannot repair a failed routing transaction.
    #[cfg(unittest)]
    pub(super) fn require_affinity_ready(&self) -> Result<(), IrqDescError> {
        if self.affinity.has_failed_programming {
            return Err(IrqDescError::ConfigurationFailed {
                hwirq: self.desc.hwirq,
            });
        }
        Ok(())
    }

    /// Like `require_affinity_ready`, but admits a masked deferred
    /// retry so activation can run the controller's recovery transaction.
    pub(super) fn require_affinity_activation_ready(&self) -> Result<(), IrqDescError> {
        if self.affinity.has_failed_programming
            && !(self.affinity.is_pending && self.is_affinity_disabled())
        {
            return Err(IrqDescError::ConfigurationFailed {
                hwirq: self.desc.hwirq,
            });
        }
        Ok(())
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
    /// Returns whether successful removal released a teardown credit. Survivor
    /// removal need not acquire one; NMI and manually gated removal still do.
    pub(super) fn finish_management(&mut self, has_removed_action: bool) -> bool {
        let has_released_teardown = has_removed_action && self.is_teardown_in_progress();
        if has_released_teardown {
            self.finish_teardown();
        }
        self.end_management();
        has_released_teardown
    }

    #[cfg(unittest)]
    pub(super) fn test_with_runtime(desc: IrqDesc, regular_action: Option<IrqAction>) -> Self {
        let mut state = Self::test_new(desc);
        if let Some(action) = regular_action {
            assert!(state.install_regular_action(action));
        }
        state
    }

    #[cfg(unittest)]
    pub(super) fn set_action_history_for_tests(&mut self) {
        self.has_action_history = true;
    }

    #[cfg(unittest)]
    pub(super) fn clear_action_history_for_tests(&mut self) {
        self.has_action_history = false;
    }

    #[cfg(unittest)]
    pub(super) fn test_new(desc: IrqDesc) -> Self {
        static CHIP: crate::FasteoiChip = crate::FasteoiChip {
            mask: |_| {},
            unmask: |_| {},
            resend: crate::FasteoiResend::Level,
        };
        let mut state = Self::new(desc);
        state.set_flow_for_tests(crate::IrqFlow::Fasteoi(&CHIP));
        state
    }

    #[cfg(unittest)]
    pub(super) fn set_flow_for_tests(&mut self, flow: crate::IrqFlow) {
        self.flow = Some(flow);
        self.test_flow = Some(flow);
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
    pub(super) const fn teardown_depth_for_tests(&self) -> usize {
        self.runtime.teardown_depth
    }
}

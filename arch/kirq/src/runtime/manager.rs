// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IRQ registration, source control, mapping and retirement transactions.
//!
//! Serializes management against membership changes on the retained Line and
//! coordinates controller plans with locked state. Ordinary retirement waits
//! for execution drain; PerCpu retirement requires caller-completed shutdown.
//! Trap-entry context and completion orchestration live in the entry module.
//!
//! Management is not NMI-safe. Checked map/enable/disable/register/free/sync
//! entry points reject NMI before locking. Compatibility wrappers also suppress
//! their usual failure log in NMI, since the logger may use ordinary locks.

use alloc::sync::Arc;

use fwnode::{FwIrqSpec, FwSource};
use kiface_task_api::TaskBlockingContextIf;
use kspin::{NoPreemptIrqSave, SpinNoIrq};
#[cfg(unittest)]
pub(super) use tests_action_helpers::{
    register_locked, register_shared_locked, take_from_line, take_from_state, withdraw_on_line,
};
#[cfg(unittest)]
pub(crate) use tests_management_identity::publish_replaced_on_acquisition;

#[cfg(unittest)]
pub(super) use super::entry::{
    dispatch_and_complete_for_tests as dispatch_and_complete, dispatch_simulated_irq,
    run_claimed_irq_tail, run_irq_exit_tail_for_tests as run_irq_exit_tail_with_context,
};
#[cfg(unittest)]
use super::state::DYNAMIC_VIRQ_BASE;
use super::{
    action::{IrqActionToken, PreparedAction},
    action_set::RetiredAction,
};
#[cfg(unittest)]
use crate::action::IrqAction;
use crate::{
    Hwirq, IrqDesc, IrqDescError, IrqDomainId, IrqFlow, IrqSource, IrqSpec, Virq,
    domain::fw,
    platform::{Handler, IntrManagerIf, configure_platform_irq, set_platform_irq_enabled},
    state::{
        IRQ_STATE, IrqLine, IrqPlatformPlan, IrqState, IrqStateDesc, resolve_prepared_locked,
        validate_resolution_domain,
    },
};

/// Retained source identity for explicit CPU-local guest completion delegation.
/// Does not own the producer or implicitly disable/drain it on Drop.
pub struct PerCpuForwarding {
    _line: Arc<IrqLine>,
    desc: IrqDesc,
    chip: &'static crate::PerCpuForwardingChip,
}

impl PerCpuForwarding {
    /// Changes delegation on the executing CPU without waiting or allocation.
    ///
    /// # Safety
    /// The source owner must retain the registered action and exclude mapping,
    /// configuration, teardown and competing ownership changes for this handle's
    /// lifetime. Call outside NMI, with IRQs and migration disabled. Before Host,
    /// stop the producer and mask delivery; before Guest, reconstruct logical
    /// activity and keep delivery masked until state installation completes.
    /// No outstanding claim may cross either ownership transition.
    pub unsafe fn set_local_state(&self, state: crate::PerCpuForwardingState) {
        debug_assert!(!karch::local_irq_enabled());
        debug_assert!(!crate::context::is_in_nmi());
        // SAFETY: preparation retained the admitted binding; this unsafe
        // method's caller supplies the source lifetime and local exclusion.
        unsafe { (self.chip.set_state)(self.desc, state) };
    }
}

/// Prepares forwarding access for an already registered CPU-local source.
///
/// Task context only. This does not activate or change hardware and does not
/// replace source-owner lifetime/exclusion obligations of `set_local_state`.
///
/// # Errors
/// Returns InvalidContext outside management context, UnknownIrq when absent,
/// NoIrqAction when the retained Line has no action,
/// or UnsupportedFlow when the controller/source cannot delegate completion.
pub fn prepare_percpu_forwarding(
    spec: impl Into<IrqSpec>,
) -> Result<PerCpuForwarding, IrqDescError> {
    can_wait_for_irq_sync("prepare_percpu_forwarding")?;
    let state = IRQ_STATE.lock();
    let Some((virq, line)) = lookup_line(&state, spec.into()) else {
        return Err(IrqDescError::UnknownIrq);
    };
    let entry = line.lock();
    if !entry.has_actions() {
        return Err(IrqDescError::NoIrqAction { virq });
    }
    let desc = entry.desc;
    if entry.flow != Some(crate::IrqFlow::PerCpu) {
        return Err(IrqDescError::UnsupportedFlow { hwirq: desc.hwirq });
    }
    let chip = IntrManagerIf::percpu_forwarding(desc)
        .ok_or(IrqDescError::UnsupportedFlow { hwirq: desc.hwirq })?;
    drop(entry);
    Ok(PerCpuForwarding {
        _line: line,
        desc,
        chip,
    })
}

/// Options for installing a regular IRQ handler.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IrqRegisterOptions {
    auto_enable: bool,
    user_affinity_restricted: bool,
}

impl IrqRegisterOptions {
    /// Register and enable the IRQ line after successful handler installation.
    pub const fn auto_enable() -> Self {
        Self {
            auto_enable: true,
            user_affinity_restricted: false,
        }
    }

    /// Register the handler but leave delivery disabled until explicit enable.
    pub const fn disabled() -> Self {
        Self {
            auto_enable: false,
            user_affinity_restricted: false,
        }
    }

    /// Declares that this regular action's source owner does not permit
    /// user-originated routing changes.
    ///
    /// This is a user-only restriction, not Linux `IRQF_NOBALANCING`: the
    /// trusted kernel setter and automatic initial routing are unchanged. The
    /// flag is published with the action and disappears with its last owner.
    pub const fn restrict_user_affinity(mut self) -> Self {
        self.user_affinity_restricted = true;
        self
    }

    /// Returns whether successful registration should enable the line.
    pub const fn should_auto_enable(self) -> bool {
        self.auto_enable
    }

    /// Returns whether this action declares a user-affinity restriction.
    pub const fn is_user_affinity_restricted(self) -> bool {
        self.user_affinity_restricted
    }
}

impl Default for IrqRegisterOptions {
    fn default() -> Self {
        Self::auto_enable()
    }
}

fn apply_platform_plan(plan: IrqPlatformPlan) -> Result<(), IrqDescError> {
    if plan.configure {
        configure_platform_irq(plan.desc)?;
    }
    apply_platform_delivery(plan);
    Ok(())
}

// Disable and withdrawal plans cannot configure a source. Their delivery
// operation retains the existing infallible mask contract.
fn apply_platform_delivery(plan: IrqPlatformPlan) {
    if let Some(on) = plan.enable {
        set_platform_irq_enabled(plan.desc, on);
    }
    if let Some(unmask) = plan.flow_unmask {
        unmask(plan.desc);
    }
}
/// A firmware interrupt specifier mapped into the logical IRQ namespace.
///
/// The descriptor is the authoritative stored resource: it includes metadata
/// from both the firmware declaration and any earlier mapping of the same
/// source. Consumers that register a handler must use [`Self::virq`]; consumers
/// that need trigger or controller details must use this descriptor rather than
/// reconstructing a default one from the logical number.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MappedIrq {
    /// OS-visible logical IRQ number.
    pub virq: Virq,
    /// Resource metadata retained by the IRQ core.
    pub desc: IrqDesc,
}

/// A firmware interrupt declaration that could not be mapped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FwIrqMapError {
    /// No IRQ domain is registered for the specifier's exact controller node.
    ControllerNotReady,
    /// The controller rejected the binding cells as malformed.
    MalformedSpec,
    /// The controller understands the binding but cannot implement this entry.
    UnsupportedSpec,
    /// The controller binding decoded an out-of-range hardware number.
    InvalidHwirq,
    /// The IRQ core rejected the resulting resource.
    Map(IrqDescError),
}

/// Try to map a hardware IRQ resource into the OS-visible logical IRQ namespace.
///
/// A Domain association retains its Line across handler removal and empty
/// dispatch. Platform mappings belong to the Domain/platform lifetime; this is
/// not a per-handler allocation. MSI allocations use their separate explicit
/// release protocol. Descriptors without a Domain retain registry lifetime too;
/// neither action removal nor dispatch completion disposes them. No general
/// non-MSI runtime disposal API is provided yet.
///
/// Requires allocation-ready boot/task context, outside IRQ/NMI/BH handlers.
/// New index entries also reject active RCU read scopes. Capacity preparation
/// failure returns `NoMemory` without publishing either entry.
pub fn try_map(desc: IrqDesc) -> Result<Virq, IrqDescError> {
    try_map_resolved(desc).map(|mapped| mapped.virq)
}

/// Maps a firmware interrupt specifier through its controller's registered
/// domain.
///
/// The specifier's controller node selects the domain by exact firmware-node
/// identity. The controller's `IrqDomainXlate` interprets the binding
/// cells; the IRQ core then associates the resulting hardware number with the
/// registered domain and allocates or reuses the logical IRQ.
///
/// A repeated mapping of the same valid declaration returns the existing
/// mapping. Conflicting hardware, domain, trigger or controller metadata is an
/// error; it is never merged by discarding one side.
///
/// # Errors
///
/// Returns [`FwIrqMapError::ControllerNotReady`] when no domain is registered
/// for the controller node, a translation error when the binding rejects the
/// cells, and [`FwIrqMapError::Map`] when the IRQ core rejects the resource
/// (including capacity and mapping conflicts).
pub fn try_map_fwspec(spec: FwIrqSpec<'static>) -> Result<MappedIrq, FwIrqMapError> {
    reject_nmi_management("map firmware IRQ").map_err(FwIrqMapError::Map)?;
    if crate::context::is_in_interrupt_context() {
        return Err(FwIrqMapError::Map(IrqDescError::InvalidContext {
            operation: "map firmware IRQ",
        }));
    }
    let binding = fw::lookup(spec.controller).ok_or(FwIrqMapError::ControllerNotReady)?;
    let hw = binding
        .xlate
        .translate(spec.params.as_slice())
        .map_err(|error| match error {
            fw::IrqXlateError::Malformed => FwIrqMapError::MalformedSpec,
            fw::IrqXlateError::Unsupported => FwIrqMapError::UnsupportedSpec,
            fw::IrqXlateError::InvalidHwirq => FwIrqMapError::InvalidHwirq,
        })?;
    let desc = IrqDesc::new(hw.hwirq, hw.trigger)
        .with_controller(binding.kind)
        .with_domain(binding.domain)
        .with_source(irq_source(spec.controller.source()));
    try_map_resolved(desc).map_err(FwIrqMapError::Map)
}

fn irq_source(source: FwSource) -> IrqSource {
    match source {
        FwSource::DeviceTree => IrqSource::DeviceTree,
        FwSource::Acpi => IrqSource::Acpi,
        FwSource::Static => IrqSource::PlatformStatic,
        FwSource::Unknown => IrqSource::Unknown,
    }
}

fn try_map_resolved(desc: IrqDesc) -> Result<MappedIrq, IrqDescError> {
    reject_nmi_management("map IRQ")?;
    if crate::context::is_in_interrupt_context() {
        return Err(IrqDescError::InvalidContext {
            operation: "map IRQ",
        });
    }
    let spec = IrqSpec::Desc(desc);
    validate_resolution_domain(spec)?;
    loop {
        let (resolution, existing, mapping) = {
            let mut state = IRQ_STATE.lock();
            let resolution = state.plan_resolution(spec)?;
            let existing = state.descs.get(&resolution.virq);
            let mapping = state.mapping_to_prepare(spec);
            (resolution, existing, mapping)
        };
        let mut prepared =
            super::index::PreparedPublication::new(resolution.virq, mapping, existing.is_none())?;
        let candidate = existing.unwrap_or_else(|| prepare_line(spec));
        // Rejected private owners are destroyed only after registry exclusion.
        let line;
        let mut state = IRQ_STATE.lock();
        if desc.logical_irq().is_none()
            && desc.domain.is_some()
            && state
                .lookup_virq(spec)
                .is_some_and(|virq| virq != resolution.virq)
        {
            continue;
        }
        let virq = resolution.virq;
        line = state.descs.get(&virq).unwrap_or_else(|| candidate.clone());
        let resolved =
            state.commit_resolution(resolution, &line, &mut line.lock(), &mut prepared)?;
        return Ok(MappedIrq {
            virq,
            desc: resolved.into_desc(),
        });
    }
}

/// Returns the mapped logical IRQ number for a domain-local hardware IRQ.
///
/// IRQ/NMI handlers borrow the Domain index in an interrupt RCU scope. Boot and
/// control callers use its writer-locked read without requiring an RCU service.
/// `None` means the domain has no mapping and no explicit identity policy.
///
/// Handler callers require balanced interrupt entry to have established RCU
/// watching and retained initialized current-task storage. This is not an
/// early architecture-entry API. Boot/control callers require initialized
/// CPU-local lock primitives, but need neither a current task nor a GP service.
///
/// # Panics
/// Handler lookup of a registered Domain panics if watching or current-task
/// storage is unavailable, or if the interrupt read nesting counter overflows.
pub fn translate_hwirq(domain: IrqDomainId, hwirq: Hwirq) -> Option<Virq> {
    crate::domain::resolve(domain, hwirq)
}

/// Configure and enable or disable an IRQ line.
///
/// The input is an [`IrqSpec`]: `usize` means a plain OS-visible `virq` with no
/// hardware metadata, while [`IrqDesc`] means a hardware IRQ resource that may
/// carry domain, trigger, polarity, controller, affinity, or flag state. A
/// plain `virq` still resolves to the stored identity descriptor used by older
/// platform-static lines, so low-numbered identity IRQs may still reach the
/// platform enable path; it simply does not contribute new metadata.
#[inline]
pub fn enable(spec: impl Into<IrqSpec>, on: bool) {
    if let Err(err) = try_enable(spec, on) {
        report_management_error("enable IRQ", &err);
    }
}

/// Try to enable or disable an IRQ line through the legacy boolean bridge.
///
/// `on == true` preserves the legacy force-enable behavior for static and
/// per-CPU bring-up sites. `on == false` follows [`try_disable_irq_nosync`], so
/// it is valid from hardirq context and does not wait for in-flight handlers.
/// Local sources update only the calling CPU's control slot. Success does
/// not certify that another CPU's local instance has been disabled.
/// Configuration errors leave logical enable/depth uncommitted and may leave
/// hardware masked. Resolved metadata is retained; successful retry restores
/// eligible delivery. Neither error nor success proves hardware-active drain.
/// Both directions require an existing descriptor. Missing Lines return
/// [`IrqDescError::UnknownIrq`] without allocation or controller operations;
/// owners must prepare them through [`try_map`] or MSI allocation first.
/// Adding a Domain binding is a task-only publication and may fail before any
/// controller operation; IRQ callers must use already-prepared metadata.
#[inline]
pub fn try_enable(spec: impl Into<IrqSpec>, on: bool) -> Result<(), IrqDescError> {
    if on {
        try_enable_irq_inner(spec.into(), true)
    } else {
        try_disable_irq_nosync(spec)
    }
}

/// Enable an IRQ line after a previous disable or disabled registration.
pub fn enable_irq(spec: impl Into<IrqSpec>) {
    if let Err(err) = try_enable_irq(spec) {
        report_management_error("enable IRQ", &err);
    }
}

/// Try to enable an IRQ line after a previous disable or disabled registration.
///
/// Configuration failure preserves enable/depth and suppresses standard-flow
/// service until configuration succeeds. Hardware writes are not rolled back.
/// A routing failure also rejects enable, before changing depth or hardware;
/// trigger reconfiguration cannot clear it. See [`crate::set_irq_affinity`] for
/// the explicit disabled-source routing recovery protocol.
/// For PerCpu flow, only the calling CPU's slot and source gate are changed;
/// CPU selection remains pinned through the hardware transaction. Local control
/// is not nested: one enable reopens the gate after any number of local disables.
pub fn try_enable_irq(spec: impl Into<IrqSpec>) -> Result<(), IrqDescError> {
    try_enable_irq_inner(spec.into(), false)
}

fn try_enable_irq_inner(spec: IrqSpec, force_platform_enable: bool) -> Result<(), IrqDescError> {
    reject_nmi_management("enable IRQ")?;
    enable_line(&IRQ_STATE, spec, force_platform_enable, apply_platform_plan)
}

fn enable_line(
    registry: &SpinNoIrq<IrqState>,
    spec: IrqSpec,
    force_platform_enable: bool,
    apply_plan: impl FnOnce(IrqPlatformPlan) -> Result<(), IrqDescError>,
) -> Result<(), IrqDescError> {
    if force_platform_enable {
        return enable_legacy_line(registry, spec, apply_plan);
    }
    let line;
    let _context_guard = NoPreemptIrqSave::new();
    let state = registry.lock();
    let virq = state.lookup_virq(spec).ok_or(IrqDescError::UnknownIrq)?;
    line = state
        .descs
        .get(&virq)
        .ok_or(IrqDescError::UnknownIrq)?
        .clone();
    let mut entry = line.lock();
    drop(state);
    let plan = prepare_enable_plan(&entry, virq, false)?;
    apply_configuration_plan(&mut entry, plan, apply_plan)?;
    entry.commit_platform_plan(plan);
    drop(entry);
    super::resend::schedule(&line);
    Ok(())
}

fn enable_legacy_line(
    registry: &SpinNoIrq<IrqState>,
    spec: IrqSpec,
    apply_plan: impl FnOnce(IrqPlatformPlan) -> Result<(), IrqDescError>,
) -> Result<(), IrqDescError> {
    // Source activation cannot allocate descriptor ownership. Keep the retained
    // owner outside the guards, and preserve IRQ exclusion across their handoff.
    let (virq, line, mapping) = {
        let state = registry.lock();
        let (virq, line) = lookup_line(&state, spec).ok_or(IrqDescError::UnknownIrq)?;
        (virq, line, state.mapping_to_prepare(spec))
    };
    if mapping.is_some() && crate::context::is_in_interrupt_context() {
        return Err(IrqDescError::InvalidContext {
            operation: "extend IRQ mapping",
        });
    }
    let mut prepared = super::index::PreparedPublication::new(virq, mapping, false)?;
    let _context_guard = NoPreemptIrqSave::new();
    let mut state = registry.lock();
    if !state
        .descs
        .get(&virq)
        .is_some_and(|current| Arc::ptr_eq(&current, &line))
    {
        return Err(IrqDescError::UnknownIrq);
    }
    let mut entry = line.lock();
    reject_teardown(&entry, virq, "enable IRQ")?;
    resolve_prepared_locked(&mut state, spec, &line, &mut entry, &mut prepared)?;
    drop(state);
    let plan = prepare_enable_plan(&entry, virq, true)?;
    apply_configuration_plan(&mut entry, plan, apply_plan)?;
    entry.commit_platform_plan(plan);
    drop(entry);
    super::resend::schedule(&line);
    Ok(())
}

// Admission, legacy metadata resolution, and the state change share the
// caller-held Line guard through the controller update.
fn prepare_enable_plan(
    entry: &IrqStateDesc,
    virq: Virq,
    force_platform_enable: bool,
) -> Result<IrqPlatformPlan, IrqDescError> {
    entry.require_affinity_activation_ready()?;
    require_bound_flow(entry)?;
    reject_teardown(entry, virq, "enable IRQ")?;
    if entry.has_nmi_action() && !entry.has_ready_local_nmi_preparation() {
        return Err(IrqDescError::ConfigurationFailed {
            hwirq: entry.desc.hwirq,
        });
    }
    if force_platform_enable {
        Ok(entry.prepare_legacy_enable())
    } else {
        if !entry.has_actions() {
            warn!("enable IRQ {virq} failed because no IRQ action is registered");
            return Err(IrqDescError::NoIrqAction { virq });
        }
        Ok(entry.prepare_enable_irq())
    }
}

/// Disable an IRQ line without waiting for an in-flight handler to finish.
/// For PerCpu flow this masks only the calling CPU's source, without accumulating
/// disable depth. Repeated calls remain disabled until one local enable.
pub fn disable_irq_nosync(spec: impl Into<IrqSpec>) {
    if let Err(err) = try_disable_irq_nosync(spec) {
        report_management_error("disable IRQ", &err);
    }
}

/// Try to disable an existing IRQ line without waiting for an in-flight handler to finish.
///
/// This is a lookup-only control operation: it never creates descriptors,
/// allocates `virq`s, or publishes domain mappings. Unknown IRQs return
/// [`IrqDescError::UnknownIrq`].
/// Ordinary Lines accumulate disable depth; PerCpu Lines simply close the
/// calling CPU's gate. Neither operation waits for source-owned shutdown.
pub fn try_disable_irq_nosync(spec: impl Into<IrqSpec>) -> Result<(), IrqDescError> {
    reject_nmi_management("disable IRQ")?;
    disable_line(&IRQ_STATE, spec.into(), false, apply_platform_delivery).map(|_| ())
}

// Backend updates are bounded and cannot call back into KIRQ management. Keep
// the selected Line locked through the mask write, but release the directory
// before entering the controller. The Line guard serializes mask with enable
// and registration updates without taking global control exclusion.
fn disable_line(
    registry: &SpinNoIrq<IrqState>,
    spec: IrqSpec,
    is_synchronous: bool,
    apply_plan: impl FnOnce(IrqPlatformPlan),
) -> Result<IrqDrain, IrqDescError> {
    // Registry-to-Line handoff releases nested IRQ-saving guards out of order.
    // Preserve the entry context until both are gone, without a global lock.
    let _context_guard = NoPreemptIrqSave::new();
    let state = registry.lock();
    let virq = state.lookup_virq(spec).ok_or(IrqDescError::UnknownIrq)?;
    let line = state.descs.get(&virq).ok_or(IrqDescError::UnknownIrq)?;
    let drain = IrqDrain::new(virq, &line);
    {
        let mut entry = drain.line.lock();
        // The outer context guard masks IRQs across this non-LIFO handoff.
        drop(state);
        require_bound_flow(&entry)?;
        if is_synchronous {
            require_non_percpu_flow(&entry)?;
        }
        let plan = entry.prepare_disable_irq_nosync();
        apply_plan(plan);
        entry.commit_platform_plan(plan);
    }
    Ok(drain)
}

/// Disable an IRQ line and wait for currently running handlers to finish.
///
/// Requires sleepable task context accepted by [`TaskBlockingContextIf::is_sleepable_context`],
/// even when no handler is currently in flight. Returns `false` for an unknown
/// descriptor, PerCpu flow or invalid context; use [`try_disable_irq`] to distinguish them.
///
/// Callers must not hold locks or resources that the IRQ handler may acquire
/// while waiting, otherwise the handler can block behind its own teardown.
pub fn disable_irq(spec: impl Into<IrqSpec>) -> bool {
    match try_disable_irq(spec) {
        Ok(done) => done,
        Err(err) => {
            report_management_error("disable IRQ", &err);
            false
        }
    }
}

/// Try to disable an IRQ line and wait for currently running handlers to finish.
///
/// Resolved PerCpu flow returns [UnsupportedFlow](IrqDescError::UnsupportedFlow)
/// before hardware or disable-depth changes. Local source owners use
/// [try_disable_irq_nosync] on each participating CPU and complete their own
/// source-stop protocol; removal uses [try_free_percpu_irq].
///
/// The drain covers admitted claims and running software replay, not pending
/// queue entries. A queued executor may run later, but must recheck Line
/// eligibility before admission or handler/chip access.
/// Level/Edge/Fasteoi rejected claims complete under the Line lock without an
/// execution credit; the drain's locked zero check serializes with that completion.
///
/// Requires sleepable task context accepted by [`TaskBlockingContextIf::is_sleepable_context`]:
/// a non-idle task, enabled IRQs, no interrupt/BH-disabled context, preemption
/// protection or explicit RCU read scope. Validation precedes masking, even
/// when no handler is in flight. Misuse returns [`IrqDescError::InvalidContext`].
///
/// Callers must not hold locks or resources that the IRQ handler may acquire
/// while waiting, otherwise the handler can block behind its own teardown.
pub fn try_disable_irq(spec: impl Into<IrqSpec>) -> Result<bool, IrqDescError> {
    let spec = spec.into();
    can_wait_for_irq_sync("disable_irq")?;
    let drain = disable_line(&IRQ_STATE, spec, true, apply_platform_delivery)?;
    drain.wait();
    Ok(true)
}

/// Wait until admitted IRQ claims and running software replay have finished.
///
/// Pending replay queue entries are not drained. They may execute after this
/// call returns if still eligible; this operation does not cancel queued work.
///
/// This API does not mask the line. For ordinary sources, call [`disable_irq`]
/// first when teardown must prevent new handler entries before waiting for
/// existing dispatches. PerCpu flow is rejected; local sources require
/// caller-owned shutdown on every relevant CPU, not a generic broadcast barrier.
///
/// Callers must not hold locks or resources that the IRQ handler may acquire
/// while waiting, otherwise the handler can block behind its own teardown.
pub fn synchronize_irq(spec: impl Into<IrqSpec>) -> bool {
    match try_synchronize_irq(spec) {
        Ok(done) => done,
        Err(err) => {
            report_management_error("synchronize IRQ", &err);
            false
        }
    }
}

/// Try to wait until admitted IRQ claims and running software replay have finished.
///
/// Resolved PerCpu flow returns [UnsupportedFlow](IrqDescError::UnsupportedFlow)
/// before waiting. This API is not a per-CPU source-stop protocol.
/// This restriction separates KIRQ's generic drain and source-owner scopes;
/// Linux's `synchronize_irq` does not impose a blanket PerCpu rejection.
///
/// Pending software replay retains the Line but owns no execution admission.
/// This call may return before its executor runs, including before executor
/// binding. Later eligible replay can still execute; this is not queue cancellation.
/// Level/Edge/Fasteoi rejections retain the Line lock through chip operations and
/// original completion without incrementing the count. The drain takes that same
/// lock to observe zero, so it cannot skip a rejected completion already in progress.
/// As with [`synchronize_irq`], this is software drain, not all-CPU local-source
/// shutdown or proof of device/guest hardware quiescence.
///
/// Requires sleepable task context accepted by [`TaskBlockingContextIf::is_sleepable_context`],
/// even when no handler is in flight. IRQ-off, preemption-protected, explicit
/// RCU read and interrupt/BH-disabled contexts are rejected before lookup with
/// [`IrqDescError::InvalidContext`].
///
/// Callers must not hold locks or resources that the IRQ handler may acquire
/// while waiting, otherwise the handler can block behind its own teardown.
pub fn try_synchronize_irq(spec: impl Into<IrqSpec>) -> Result<bool, IrqDescError> {
    can_wait_for_irq_sync("synchronize_irq")?;
    let drain = {
        let state = IRQ_STATE.lock();
        let Some(virq) = state.lookup_virq(spec.into()) else {
            return Ok(false);
        };
        if let Some(line) = state.descs.get(&virq) {
            require_non_percpu_flow(&line.lock())?;
            Some(IrqDrain::new(virq, &line))
        } else {
            None
        }
    };
    if let Some(drain) = drain {
        drain.wait();
    }
    Ok(true)
}

fn can_wait_for_irq_sync(api: &'static str) -> Result<(), IrqDescError> {
    reject_nmi_management(api)?;
    if crate::context::is_in_interrupt_context() || !TaskBlockingContextIf::is_sleepable_context() {
        warn!("{api} called from non-sleepable IRQ context");
        return Err(IrqDescError::InvalidContext { operation: api });
    }
    Ok(())
}

// NMI can interrupt a holder of any ordinary IRQ lock. Reject before touching
// locks or logging; this is independent of whether the operation would sleep.
fn reject_nmi_management(operation: &'static str) -> Result<(), IrqDescError> {
    if crate::context::is_in_nmi() {
        return Err(IrqDescError::InvalidContext { operation });
    }
    Ok(())
}

fn report_management_error(operation: &str, error: &IrqDescError) {
    // Compatibility wrappers must not turn the checked API's lock-free NMI
    // rejection into a logger lock acquisition on the interrupted CPU.
    if !crate::context::is_in_nmi() {
        warn!("{operation} failed: {error:?}");
    }
}

/// Captures one descriptor instance while its lookup/mutation is serialized.
///
/// The retained Line owns the predicate even after directory withdrawal; the
/// number is only diagnostic metadata, not a way to recover the wait target.
/// This waits for ordinary running ownership, not queued replay or PerCpu
/// execution. Completion wakes only prompt another locked predicate check.
struct IrqDrain {
    virq: Virq,
    line: Arc<IrqLine>,
}

impl IrqDrain {
    fn new(virq: Virq, line: &Arc<IrqLine>) -> Self {
        Self {
            virq,
            line: line.clone(),
        }
    }

    fn wait(&self) {
        loop {
            let (running, completion) = {
                let entry = self.line.lock();
                if !entry.has_active_execution() {
                    return;
                }
                // Diagnostic only: the locked activity predicate is authoritative.
                let running = usize::from(entry.is_running);
                (running, entry.execution_idle_completion())
            };
            observe_wait_for_running_for_tests(self.virq, running);
            #[expect(
                clippy::expect_used,
                reason = "CG-021-no-panic-on-reachable-failure: all callers validate can_wait \
                          before mutation; scoped IRQ locks restore that context before this \
                          call, with no intervening sleep-context change; registration cannot fail"
            )]
            kiface_task_wait_api::wait_for_completion(&completion)
                .expect("validated IRQ synchronization context must remain sleepable");
        }
    }
}

#[cfg(unittest)]
type WaitForRunningHook = fn(Virq, usize);

#[cfg(unittest)]
#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to Clippy"
)]
#[allow(
    unknown_lints,
    unregistered_static_spinlock,
    reason = "CG-001-static-spinlock-registration: test-only lock; not tracked in lock_stat"
)]
static WAIT_FOR_RUNNING_HOOK: SpinNoIrq<Option<WaitForRunningHook>> = SpinNoIrq::new(None);

#[cfg(unittest)]
fn observe_wait_for_running_for_tests(virq: Virq, running: usize) {
    let hook = *WAIT_FOR_RUNNING_HOOK.lock();
    if let Some(hook) = hook {
        hook(virq, running);
    }
}

#[cfg(not(unittest))]
#[inline(always)]
fn observe_wait_for_running_for_tests(_virq: Virq, _running: usize) {}

/// Return the descriptor currently remembered for an IRQ line, if any.
pub fn descriptor(virq: Virq) -> Option<IrqDesc> {
    IRQ_STATE.lock().stored_desc(virq)
}

/// Register the regular OS IRQ handler for an IRQ line.
///
/// Each registration carries its own `Arc<dyn IrqHandler>` — the Rust-native
/// counterpart of Linux's `dev_id` — and is stored internally as the line's
/// sole regular IRQ action. The public API still accepts only one regular
/// handler per IRQ line.
///
/// The handler runs directly on dispatch. Device subsystems own deferred work
/// and wake their readers when data becomes ready.
/// Registration requires sleepable task context, including when uncontended;
/// boot callers use [`try_register_startup`] after installing the current task.
/// The source owner must prepare the descriptor through [`try_map`] or MSI
/// allocation before requesting a handler.
/// PerCpu sources use [`try_register_percpu`] instead; see
/// [`try_register_with_options`] for flow restrictions and failure semantics.
pub fn register(spec: impl Into<IrqSpec>, handler: Handler) -> bool {
    match try_register(spec, handler) {
        Ok(registered) => registered,
        Err(err) => {
            report_management_error("register IRQ handler", &err);
            false
        }
    }
}

/// Try to register the regular OS IRQ handler for an IRQ line.
///
/// Uses the sleepable management contract of [`try_register_with_options`].
pub fn try_register(spec: impl Into<IrqSpec>, handler: Handler) -> Result<bool, IrqDescError> {
    try_register_with_options(spec, handler, IrqRegisterOptions::auto_enable())
}

/// Register the regular OS IRQ handler without automatically enabling delivery.
///
/// Requires sleepable task context, as does [`register`].
pub fn register_disabled(spec: impl Into<IrqSpec>, handler: Handler) -> bool {
    match try_register_disabled(spec, handler) {
        Ok(registered) => registered,
        Err(err) => {
            report_management_error("register disabled IRQ handler", &err);
            false
        }
    }
}

/// Try to register the regular OS IRQ handler without automatically enabling delivery.
///
/// Uses the sleepable management contract of [`try_register_with_options`].
pub fn try_register_disabled(
    spec: impl Into<IrqSpec>,
    handler: Handler,
) -> Result<bool, IrqDescError> {
    try_register_with_options(spec, handler, IrqRegisterOptions::disabled())
}

/// Try to register the regular OS IRQ handler with explicit lifecycle options.
///
/// Requires sleepable task context even without contention. Management exclusion
/// serializes with registration/removal on this Line and may block or migrate.
/// Prepares NOWAIT action storage before lookup and short locks. Allocation
/// failure returns [`IrqDescError::NoMemory`] without publishing an action.
/// Requires a descriptor prepared through [`try_map`] or MSI allocation.
/// A missing descriptor returns [`IrqDescError::UnknownIrq`] without creating
/// a Line, configuring hardware or retaining a handler in the registry.
/// An occupied Line returns false before merging descriptor metadata or
/// publishing mappings; the existing action and control state remain unchanged.
/// Stale controller configuration is applied before publishing the handler,
/// including with disabled options. Those options suppress delivery activation,
/// not request-time configuration.
/// If compatibility bring-up already enabled an actionless ordinary Line,
/// disabled registration masks it before committing the disabled state.
/// PerCpu flow is rejected with [UnsupportedFlow](IrqDescError::UnsupportedFlow)
/// before configuration, action publication or disable-depth changes. Use
/// [try_register_percpu] for local sources. The check runs both before metadata
/// resolution and on the resolved binding; post-resolution rejection may retain
/// descriptor/mapping metadata but never configures the source.
/// Configuration errors publish no new handler and leave control state
/// uncommitted, but resolved descriptor/mapping metadata may remain. The
/// controller may have masked or partially programmed the source; retry is
/// explicit and an error is not a hardware rollback or drain guarantee.
pub fn try_register_with_options(
    spec: impl Into<IrqSpec>,
    handler: Handler,
    options: IrqRegisterOptions,
) -> Result<bool, IrqDescError> {
    register_with_mode(
        spec.into(),
        handler,
        RegistrationKind::Regular(options),
        ManagementMode::Task,
    )
}

/// Registers during explicit startup after current-task installation.
///
/// Never waits for management exclusion. Returns false on contention or an
/// occupied Line. IRQs may be masked, but interrupt/BH context is rejected.
/// Call before source activation; this is not an atomic-context fallback.
/// Requires a descriptor already prepared through [`try_map`] or MSI allocation.
/// NOWAIT action allocation can return [`IrqDescError::NoMemory`] before lookup;
/// publication itself does not require a running RCU grace-period service.
/// Missing descriptors return [`IrqDescError::UnknownIrq`] without creating a
/// Line, configuring hardware or retaining the offered handler.
/// Configuration errors have the same retained-metadata and no-new-handler
/// semantics as [`try_register_with_options`].
/// PerCpu flow is rejected; use [try_register_percpu_startup] instead.
pub fn try_register_startup(
    spec: impl Into<IrqSpec>,
    handler: Handler,
    options: IrqRegisterOptions,
) -> Result<bool, IrqDescError> {
    register_with_mode(
        spec.into(),
        handler,
        RegistrationKind::Regular(options),
        ManagementMode::Startup,
    )
}

/// Registers one CPU-local handler without enabling delivery on any CPU.
///
/// Requires sleepable task context and a descriptor prepared through [`try_map`].
/// The resolved controller flow must be [`IrqFlow::PerCpu`]; otherwise returns
/// [`IrqDescError::UnsupportedFlow`]. An occupied Line or any enabled local slot
/// returns false without configuring hardware or publishing the handler.
/// Flow and enabled-slot checks use the resolved binding, so their rejection
/// may retain descriptor/mapping metadata just like a configuration failure.
/// Successful registration configures only the calling CPU; each participating
/// CPU must explicitly enable its source afterward. It does not stop producers
/// or establish that an already disabled source has no outstanding claims.
/// Allocation and configuration failures follow [`try_register_with_options`].
/// Removal uses unsafe [`try_free_percpu_irq`] after the source owner has
/// completed shutdown and excluded every dispatch through its return.
pub fn try_register_percpu(
    spec: impl Into<IrqSpec>,
    handler: Handler,
) -> Result<bool, IrqDescError> {
    register_with_mode(
        spec.into(),
        handler,
        RegistrationKind::PerCpu,
        ManagementMode::Task,
    )
}

/// Registers a disabled CPU-local handler during explicit startup.
///
/// Uses [`try_register_percpu`]'s flow and disabled-slot requirements with
/// [`try_register_startup`]'s nonwaiting context contract. Source activation
/// remains a separate operation on each participating CPU.
pub fn try_register_percpu_startup(
    spec: impl Into<IrqSpec>,
    handler: Handler,
) -> Result<bool, IrqDescError> {
    register_with_mode(
        spec.into(),
        handler,
        RegistrationKind::PerCpu,
        ManagementMode::Startup,
    )
}

/// Flow-specific registration policy checked again after binding resolution.
#[derive(Clone, Copy)]
enum RegistrationKind {
    Regular(IrqRegisterOptions),
    PerCpu,
}

impl RegistrationKind {
    fn options(self, entry: &IrqStateDesc) -> Result<Option<IrqRegisterOptions>, IrqDescError> {
        match self {
            Self::Regular(options) => {
                require_non_percpu_flow(entry)?;
                Ok(Some(options))
            }
            Self::PerCpu if entry.flow != Some(IrqFlow::PerCpu) => {
                Err(IrqDescError::UnsupportedFlow {
                    hwirq: entry.desc.hwirq,
                })
            }
            Self::PerCpu if entry.has_enabled_local_cpu() => Ok(None),
            Self::PerCpu => Ok(Some(IrqRegisterOptions::disabled())),
        }
    }
}

fn register_with_mode(
    spec: IrqSpec,
    handler: Handler,
    kind: RegistrationKind,
    mode: ManagementMode,
) -> Result<bool, IrqDescError> {
    // Keep a caller-owned reference until the scoped mutex has been released,
    // including rejection and installation failure.
    validate_management(mode)?;
    let mut prepared = PreparedAction::regular(handler.clone())?;
    let result = with_registration_line_checked(
        spec,
        mode,
        |entry| {
            if matches!(kind, RegistrationKind::Regular(_)) {
                require_non_percpu_flow(entry)?;
            }
            Ok(entry.can_install_regular_action())
        },
        |virq, _, entry| {
            // Resolution may refine the descriptor and select its flow. Check
            // that final binding before configuration or action publication.
            let Some(options) = kind.options(entry)? else {
                return Ok(false);
            };
            register_prepared_locked(virq, entry, &mut prepared, options, apply_platform_plan)
        },
    );
    drop(prepared);
    drop(handler);
    if matches!(&result, Ok(None)) && !crate::context::is_in_nmi() {
        warn!("register handler for IRQ {spec:?} failed");
    }
    result.map(|registered| registered.unwrap_or(false))
}

// The caller retains Line exclusion from resolution through this update.
// Management admission pins membership after registry exclusion is released.
// The caller retains handler ownership until management exclusion is released.
fn register_prepared_locked(
    virq: Virq,
    entry: &mut IrqStateDesc,
    prepared: &mut PreparedAction,
    options: IrqRegisterOptions,
    mut apply_plan: impl FnMut(IrqPlatformPlan) -> Result<(), IrqDescError>,
) -> Result<bool, IrqDescError> {
    require_bound_flow(entry)?;
    if !entry.can_install_regular_action() {
        warn!("register handler for IRQ {virq} failed");
        return Ok(false);
    }
    let needs_restore = configure_registration(entry, &mut apply_plan)?;
    super::affinity::apply_deferred_before_activation(entry)?;
    if options.is_user_affinity_restricted() {
        prepared.set_user_affinity_restricted();
    }
    let installed = entry.publish_regular_action(prepared);
    debug_assert!(installed, "duplicate regular action was checked above");
    let mut plan = if options.should_auto_enable() {
        entry.prepare_auto_enable()
    } else {
        entry.prepare_register_disabled()
    };
    if needs_restore {
        // A disabled recovery must remember the hardware mask in logical
        // state too, so its later explicit enable cannot omit the write.
        plan.enable = Some(options.should_auto_enable());
    }
    apply_plan(plan)?;
    entry.commit_platform_plan(plan);
    Ok(true)
}

// Configuration must precede new action ownership, while delivery restoration
// must use the installed action. Keep both stages under the caller's Line guard.
fn configure_registration(
    entry: &mut IrqStateDesc,
    apply_plan: &mut impl FnMut(IrqPlatformPlan) -> Result<(), IrqDescError>,
) -> Result<bool, IrqDescError> {
    entry.require_affinity_activation_ready()?;
    let needs_restore = entry.needs_configuration_restore();
    let plan = entry.prepare_reconfigure_if_stale();
    if plan.configure {
        apply_configuration_plan(entry, plan, apply_plan)?;
        entry.commit_platform_plan(plan);
    }
    Ok(needs_restore)
}

fn apply_configuration_plan(
    entry: &mut IrqStateDesc,
    plan: IrqPlatformPlan,
    apply_plan: impl FnOnce(IrqPlatformPlan) -> Result<(), IrqDescError>,
) -> Result<(), IrqDescError> {
    super::affinity::apply_deferred_before_activation(entry)?;
    if let Err(error) = apply_plan(plan) {
        if plan.configure {
            entry.mark_configuration_failed();
        }
        return Err(error);
    }
    Ok(())
}

/// Register one shared IRQ action and return its removal token.
///
/// All actions already installed on the line must be shared actions. The first
/// shared registration configures/enables the line with default auto-enable
/// behavior; later shared registrations only add another action to the
/// core-owned fanout list.
/// PerCpu flow is not shared; see [`try_register_shared`] for checked errors.
pub fn register_shared(spec: impl Into<IrqSpec>, handler: Handler) -> Option<IrqActionToken> {
    match try_register_shared(spec, handler) {
        Ok(token) => token,
        Err(err) => {
            report_management_error("register shared IRQ handler", &err);
            None
        }
    }
}

/// Try to register one shared IRQ action with default auto-enable behavior.
///
/// PerCpu flow is rejected with [UnsupportedFlow](IrqDescError::UnsupportedFlow),
/// both before metadata resolution and on the resolved binding, before hardware
/// configuration or action publication. There is no shared PerCpu registration.
///
/// Requires sleepable task context; waits for this Line's management mutex.
/// NOWAIT action allocation precedes lookup and short locks; failure returns
/// [`IrqDescError::NoMemory`] without changing membership or delivery state.
/// Requires an already prepared descriptor; absence returns
/// [`IrqDescError::UnknownIrq`] without creating a Line or retaining the handler.
/// Incompatible actions or exhausted publication sequences return
/// None before descriptor metadata or mappings are changed.
/// Configuration errors preserve existing shared owners and publish no new
/// token. Resolved metadata can remain and hardware may be masked; a successful
/// retry restores delivery only when logical enablement and disable depth allow.
pub fn try_register_shared(
    spec: impl Into<IrqSpec>,
    handler: Handler,
) -> Result<Option<IrqActionToken>, IrqDescError> {
    let spec = spec.into();
    validate_management(ManagementMode::Task)?;
    let mut prepared = PreparedAction::shared(handler.clone())?;
    let result = with_registration_line_checked(
        spec,
        ManagementMode::Task,
        |entry| {
            require_non_percpu_flow(entry)?;
            Ok(entry.can_install_shared_action())
        },
        |virq, _, entry| {
            register_shared_prepared_locked(virq, entry, &mut prepared, apply_platform_plan)
        },
    );
    drop(prepared);
    drop(handler);
    if matches!(&result, Ok(None)) && !crate::context::is_in_nmi() {
        warn!("register shared handler for IRQ {spec:?} failed");
    }
    result.map(Option::flatten)
}

fn register_shared_prepared_locked(
    virq: Virq,
    entry: &mut IrqStateDesc,
    prepared: &mut PreparedAction,
    mut apply_plan: impl FnMut(IrqPlatformPlan) -> Result<(), IrqDescError>,
) -> Result<Option<IrqActionToken>, IrqDescError> {
    require_non_percpu_flow(entry)?;
    if !entry.can_install_shared_action() {
        warn!("register shared handler for IRQ {virq} failed");
        return Ok(None);
    }
    let needs_restore = configure_registration(entry, &mut apply_plan)?;
    super::affinity::apply_deferred_before_activation(entry)?;
    let was_first_action = !entry.has_actions();
    let Some(token) = entry.publish_shared_action(prepared) else {
        warn!("register shared handler for IRQ {virq} failed");
        return Ok(None);
    };
    let mut plan = if was_first_action {
        entry.prepare_auto_enable()
    } else {
        entry.prepare_reconfigure_if_stale()
    };
    if needs_restore {
        plan.enable = Some(true);
    }
    apply_plan(plan)?;
    entry.commit_platform_plan(plan);
    Ok(Some(token))
}

/// Selects sleepable management or startup's nonwaiting admission contract.
/// Atomic runtime callers never fall back to startup admission.
#[derive(Clone, Copy)]
pub(super) enum ManagementMode {
    Task,
    Startup,
}

pub(super) fn validate_management(mode: ManagementMode) -> Result<(), IrqDescError> {
    reject_nmi_management("IRQ management")?;
    match mode {
        ManagementMode::Task => can_wait_for_irq_sync("IRQ management"),
        ManagementMode::Startup => {
            if crate::context::is_in_interrupt_context()
                || !kiface_task_api::TaskExecutionIf::has_current_task()
            {
                return Err(IrqDescError::InvalidContext {
                    operation: "startup IRQ management",
                });
            }
            Ok(())
        }
    }
}

fn prepare_line(spec: IrqSpec) -> Arc<IrqLine> {
    let desc = match spec {
        IrqSpec::PlainVirq(virq) => IrqDesc::from_virq(virq),
        IrqSpec::Desc(desc) => desc,
    };
    Arc::new(IrqLine::new(IrqStateDesc::new(desc)))
}

/// Mirrors resolution's identity lookup without merging or publishing metadata.
fn lookup_line(state: &IrqState, spec: IrqSpec) -> Option<(Virq, Arc<IrqLine>)> {
    let virq = state.lookup_virq(spec).or_else(|| match spec {
        IrqSpec::Desc(desc) if desc.domain.is_none() => Some(desc.hwirq),
        _ => None,
    })?;
    state.descs.get(&virq).map(|line| (virq, line.clone()))
}

fn acquire_management(
    line: &Arc<IrqLine>,
    mode: ManagementMode,
) -> Result<Option<kmutex::MutexGuard<'_, ()>>, IrqDescError> {
    validate_management(mode)?;
    #[cfg(unittest)]
    tests_management_identity::replace_before_management_acquire(line);
    Ok(match mode {
        ManagementMode::Task => Some(line.management.lock()),
        ManagementMode::Startup => line.management.try_lock(),
    })
}

/// Management membership pin for one retained Line, separate from its mutex.
///
/// The caller keeps the management mutex guard alive until this drops. Normal
/// completion only releases membership admission; action-removal completion is
/// explicit and never inferred from an arbitrary management result.
struct ManagementAdmission<'a> {
    line: &'a Arc<IrqLine>,
    is_active: bool,
}

impl ManagementAdmission<'_> {
    /// Completes a successful withdrawal after its execution drain. Keep the
    /// removed owner alive until the enclosing management mutex is released.
    fn finish_action_removal(&mut self) {
        debug_assert!(self.is_active);
        finish_management_on_line(self.line, true, |unmask, desc| unmask(desc));
        self.is_active = false;
        super::resend::schedule(self.line);
    }
}

impl Drop for ManagementAdmission<'_> {
    fn drop(&mut self) {
        if !self.is_active {
            return;
        }
        self.line.lock().end_management();
    }
}

fn finish_management_on_line(
    line: &Arc<IrqLine>,
    has_removed_action: bool,
    unmask: impl FnOnce(fn(IrqDesc), IrqDesc),
) {
    // Action ownership is independent of descriptor lifetime. Retain the
    // original Line and management mutex through completion and restoration;
    // no directory lookup, withdrawal or non-LIFO lock handoff is needed.
    let mut entry = line.lock();
    let has_released_teardown = entry.finish_management(has_removed_action);
    if !has_released_teardown {
        return;
    }
    // Restore only a transaction that gated dispatch. Ungated survivors own
    // their normal flow mask/unmask interval, including any running handler.
    if entry.can_run_actions()
        && let Some(unmask_source) = entry.flow.and_then(IrqFlow::unmask)
    {
        unmask(unmask_source, entry.desc);
    }
}

/// Checks registration-specific rejection before merging or publishing metadata.
///
/// Preflight and resolution run under management, registry and Line
/// exclusion. Neither callback may reacquire outer locks, allocate or wait.
/// False rejects without resolution or publication. The body borrows the same
/// retained Line and locked state after registry release; its NMI owner can be
/// published without creating a second handler owner or another writer lock.
/// Identity retries happen before publication. Missing descriptors return
/// UnknownIrq in both modes. None means startup contention, teardown or preflight
/// rejection. Return discarded handler owners instead of
/// finally destroying them under management or short-lock exclusion.
#[expect(
    clippy::expect_used,
    reason = "CG-021-no-panic-on-reachable-failure: successful resolution assigns a virq"
)]
pub(super) fn with_registration_line_checked<T>(
    spec: IrqSpec,
    mode: ManagementMode,
    mut preflight_fn: impl FnMut(&IrqStateDesc) -> Result<bool, IrqDescError>,
    body_fn: impl FnOnce(Virq, &IrqLine, &mut IrqStateDesc) -> Result<T, IrqDescError>,
) -> Result<Option<T>, IrqDescError> {
    validate_management(mode)?;
    loop {
        let (virq, line, mapping) = {
            let state = IRQ_STATE.lock();
            let (virq, line) = lookup_line(&state, spec).ok_or(IrqDescError::UnknownIrq)?;
            (virq, line, state.mapping_to_prepare(spec))
        };
        let mut prepared = super::index::PreparedPublication::new(virq, mapping, false)?;
        let Some(_management_guard) = acquire_management(&line, mode)? else {
            return Ok(None);
        };
        let mut admission = ManagementAdmission {
            line: &line,
            is_active: false,
        };
        // Keep local context protection across the non-LIFO registry/Line
        // handoff. Management exclusion still outlives admission cleanup.
        let _context_guard = NoPreemptIrqSave::new();
        let mut state = IRQ_STATE.lock();
        match lookup_line(&state, spec) {
            Some((_, current)) if Arc::ptr_eq(&current, &line) => {}
            _ => continue,
        }
        let mut entry = line.lock();
        if entry.is_retired() {
            return Err(IrqDescError::UnknownIrq);
        }
        if entry.is_teardown_in_progress() || !preflight_fn(&entry)? {
            return Ok(None);
        }
        entry.begin_management();
        admission.is_active = true;
        let desc = resolve_prepared_locked(&mut state, spec, &line, &mut entry, &mut prepared)?;
        let virq = desc.logical_irq().expect("resolved descriptor has a virq");
        drop(state);
        let result = body_fn(virq, &line, &mut entry).map(Some);
        drop(entry);
        if result.is_ok() {
            super::resend::schedule(&line);
        }
        return result;
    }
}

/// Runs removal on the originally selected Line, never a same-number successor.
///
/// Missing/stale entries and startup contention return None without executing
/// the body. Admission pins membership across drain. The body returns Some only
/// after successful removal; only then is any acquired teardown credit released
/// under the retained Line lock, while management exclusion is still held.
/// All outcomes retain directory membership. Return removed owners out of the
/// body so their final destruction occurs after mutex release.
/// A body that acquires teardown must leave its credit set on successful removal;
/// completion releases it with admission under the Line lock. Ordinary removal
/// with surviving actions does not acquire teardown and keeps dispatch enabled.
pub(super) fn with_action_removal<T>(
    spec: IrqSpec,
    mode: ManagementMode,
    body_fn: impl FnOnce(Virq, &Arc<IrqLine>) -> Result<Option<T>, IrqDescError>,
) -> Result<Option<T>, IrqDescError> {
    with_line_admission(spec, mode, |virq, admission| {
        let removed = body_fn(virq, admission.line)?;
        if removed.is_some() {
            admission.finish_action_removal();
        }
        Ok(removed)
    })
    .map(Option::flatten)
}

/// Runs a lookup-only transaction with retained identity and management exclusion.
/// Missing/stale entries or startup contention return None without invoking the
/// body. Successful values have no action-removal semantics. All exit paths
/// release admission without changing teardown, flow masking or pending service.
/// Return destructor-bearing values so they outlive the management mutex.
pub(super) fn with_existing_line<T>(
    spec: IrqSpec,
    mode: ManagementMode,
    body_fn: impl FnOnce(Virq, &Arc<IrqLine>) -> Result<T, IrqDescError>,
) -> Result<Option<T>, IrqDescError> {
    with_line_admission(spec, mode, |virq, admission| body_fn(virq, admission.line))
}

fn with_line_admission<T>(
    spec: IrqSpec,
    mode: ManagementMode,
    body_fn: impl FnOnce(Virq, &mut ManagementAdmission<'_>) -> Result<T, IrqDescError>,
) -> Result<Option<T>, IrqDescError> {
    validate_management(mode)?;
    let selected = {
        let state = IRQ_STATE.lock();
        lookup_line(&state, spec)
    };
    let Some((virq, line)) = selected else {
        return Ok(None);
    };
    admit_retained_line(virq, &line, mode, body_fn)
}

/// Runs a control transaction against a caller-retained Line identity.
///
/// The management mutex is acquired before the registry lock. The expected Arc
/// must still be the directory entry for `virq`; otherwise the operation returns
/// `None` without invoking the body or touching a same-number successor. After
/// registry and Line release, the body runs with management admission only; it
/// must acquire the Line lock itself for state or hardware access. The mutex is
/// held across membership begin/end, while `is_managed` prevents eligible
/// retirement between short-lock scopes.
pub(super) fn with_retained_line<T>(
    virq: Virq,
    line: &Arc<IrqLine>,
    mode: ManagementMode,
    body_fn: impl FnOnce(Virq, &Arc<IrqLine>) -> Result<T, IrqDescError>,
) -> Result<Option<T>, IrqDescError> {
    validate_management(mode)?;
    admit_retained_line(virq, line, mode, |virq, admission| {
        body_fn(virq, admission.line)
    })
}

fn admit_retained_line<T>(
    virq: Virq,
    line: &Arc<IrqLine>,
    mode: ManagementMode,
    body_fn: impl FnOnce(Virq, &mut ManagementAdmission<'_>) -> Result<T, IrqDescError>,
) -> Result<Option<T>, IrqDescError> {
    let Some(_management_guard) = acquire_management(line, mode)? else {
        return Ok(None);
    };
    let mut admission = ManagementAdmission {
        line,
        is_active: false,
    };
    {
        let state = IRQ_STATE.lock();
        if !state
            .descs
            .get(&virq)
            .is_some_and(|current| Arc::ptr_eq(&current, line))
        {
            return Ok(None);
        }
        let mut entry = line.lock();
        if entry.is_retired() {
            return Ok(None);
        }
        entry.begin_management();
        admission.is_active = true;
    }
    body_fn(virq, &mut admission).map(Some)
}

fn reject_teardown(
    entry: &IrqStateDesc,
    virq: Virq,
    operation: &'static str,
) -> Result<(), IrqDescError> {
    if entry.is_teardown_in_progress() || entry.is_nmi_closing() {
        warn!("{operation} for IRQ {virq} failed while teardown is in progress");
        return Err(IrqDescError::TeardownInProgress { virq });
    }
    Ok(())
}

/// Remove the regular OS IRQ handler for an IRQ line.
///
/// This is a compatibility wrapper for [`free_irq`].
pub fn unregister(spec: impl Into<IrqSpec>) -> Option<Handler> {
    free_irq(spec)
}

/// Remove the regular OS IRQ handler and wait for in-flight dispatch to finish.
///
/// An NMI-owned action is not a regular registration and cannot be removed
/// here. Its owner must use [`crate::unregister_nmi`] after quiescing the source.
/// All descriptor identities survive action removal, including domainless
/// Lines. This is not a mapping-disposal operation; MSI owners subsequently
/// use [`crate::free_msix`].
///
/// The line is masked once the last regular action is removed. The API
/// returns after previously admitted handler snapshots and their original host
/// claim completions have finished. This is software drain, not proof that a
/// device producer or guest-controlled deactivation has stopped. Reclaiming
/// MMIO/DMA resources requires that separate owner-side shutdown.
/// Pending replay queue entries are not drained or eagerly cancelled. A later
/// executor takes admission and a fresh snapshot only after locked eligibility
/// checks, so it cannot borrow the removed action. Its retained Line Arc protects
/// descriptor storage, not device resources or permission to access a retired chip.
/// Pending service is Line-scoped, not an action-registration epoch: after later
/// registration on the same live Line, delayed replay may run a newly eligible
/// action, never the removed action. Free does not cancel that pending service.
/// PerCpu flow is rejected without withdrawing the action; use
/// [try_free_percpu_irq] after caller-completed local shutdown on every relevant
/// CPU and exclusion of re-enable. This function neither performs broadcast
/// shutdown nor tracks online CPU membership. Other flows retain their own
/// shutdown rules.
/// A completed CPU call can establish local stop completion, but cannot own
/// this source's re-enable gate or CPU membership. Keep that owner-side gate
/// across stop and removal; if any stop fails, retain the handler and resources.
///
/// Callers must not hold locks or resources that the IRQ handler may acquire
/// while waiting, otherwise the handler can block behind its own teardown.
///
/// Requires sleepable task context accepted by [`TaskBlockingContextIf::is_sleepable_context`],
/// even when no handler is in flight. Invalid context is rejected before
/// action removal and returns `None`; use [`try_free_irq`] to distinguish
/// context errors from absence. `None` is not proof of successful teardown.
pub fn free_irq(spec: impl Into<IrqSpec>) -> Option<Handler> {
    match try_free_irq(spec) {
        Ok(handler) => handler,
        Err(err) => {
            report_management_error("free IRQ handler", &err);
            None
        }
    }
}

/// Try to remove the regular OS IRQ handler and synchronize dispatch teardown.
///
/// Resolved PerCpu flow returns [UnsupportedFlow](IrqDescError::UnsupportedFlow)
/// before hardware, action or disable-depth changes; use [try_free_percpu_irq].
///
/// The source-shutdown and resource-lifetime obligations of [`free_irq`] apply.
///
/// Pending replay entries are not drained; a later executor cannot snapshot the
/// removed action. Already admitted claims and running replay must finish.
///
/// Requires sleepable task context accepted by [`TaskBlockingContextIf::is_sleepable_context`],
/// even when no handler is in flight. Context misuse is reported as
/// [`IrqDescError::InvalidContext`] without removing the action.
///
/// Callers must not hold locks or resources that the IRQ handler may acquire
/// while waiting, otherwise the handler can block behind its own teardown.
///
/// Context is checked before action removal. Unlinking and execution drain
/// require no action-retirement allocation or RCU grace-period wait. The task
/// waits for admitted original claims and running replay to finish before returning the handler.
pub fn try_free_irq(spec: impl Into<IrqSpec>) -> Result<Option<Handler>, IrqDescError> {
    try_free_irq_inner(spec.into(), None)
}

/// Removes an ordinary PerCpu handler after source-owned shutdown.
///
/// The resolved Line must have [PerCpu](IrqFlow::PerCpu) flow; another flow
/// returns [UnsupportedFlow](IrqDescError::UnsupportedFlow) without changing
/// action or delivery state. Any enabled local slot returns `Ok(None)`.
/// Absence, shared registration and NMI ownership also return `Ok(None)`;
/// this API removes only a regular action. The mapping remains installed.
///
/// Like Linux's `free_percpu_irq`, the source owner must stop the source on all
/// participating CPUs and exclude restart throughout removal. Disabled software
/// slots do not prove producer stop or hardware quiescence. This operation does
/// not broadcast shutdown or establish CPU membership.
/// It also performs no local mask operation; shutdown is a prerequisite, not
/// a side effect of removing the action.
///
/// Requires sleepable task context. This operation does not wait for executing
/// handlers or controller completion, nor allocate an action retirement record.
///
/// # Safety
/// The source owner must have stopped delivery on every participating CPU and
/// waited for every admitted dispatch to return, including its handler and
/// original controller completion. This includes software dispatch. It must
/// exclude restart and new dispatch until
/// this call returns. A handler clearing its own enabled slot is not proof of
/// completed execution. A failed shutdown must retain the action for retry.
/// These obligations also apply when the enabled set is already empty; merely
/// submitting remote stop callbacks does not satisfy them.
pub unsafe fn try_free_percpu_irq(
    spec: impl Into<IrqSpec>,
) -> Result<Option<Handler>, IrqDescError> {
    can_wait_for_irq_sync("free_percpu_irq")?;
    with_action_removal(spec.into(), ManagementMode::Task, |virq, line| {
        let Some(retired) =
            withdraw_action_on_line(virq, line, None, true, apply_platform_delivery)?
        else {
            return Ok(None);
        };
        // SAFETY: the caller completed all PerCpu execution and excludes new
        // dispatch. Unlink holds the Line lock; management excludes writers.
        let removed = unsafe { retired.reclaim() };
        Ok(Some(removed.into_primary()))
    })
}

/// Remove one shared IRQ action by token and synchronize dispatch teardown.
///
/// The source-shutdown and resource-lifetime obligations of [`free_irq`] apply.
/// Removing one shared owner does not disable surviving actions.
///
/// Callers must not hold locks or resources that the IRQ handler may acquire
/// while waiting, otherwise the handler can block behind its own teardown.
///
/// Requires sleepable task context accepted by [`TaskBlockingContextIf::is_sleepable_context`],
/// even when no handler is in flight. Invalid context leaves the action intact
/// and returns `None`; use [`try_free_irq_action`] to distinguish errors from
/// absence. `None` is not proof of successful teardown.
pub fn free_irq_action(spec: impl Into<IrqSpec>, token: IrqActionToken) -> Option<Handler> {
    match try_free_irq_action(spec, token) {
        Ok(handler) => handler,
        Err(err) => {
            report_management_error("free shared IRQ handler", &err);
            None
        }
    }
}

/// Try to remove one shared IRQ action by token and synchronize dispatch teardown.
///
/// Resolved PerCpu flow returns [UnsupportedFlow](IrqDescError::UnsupportedFlow)
/// before hardware, action or disable-depth changes, regardless of the token.
///
/// The source-shutdown and resource-lifetime obligations of [`free_irq_action`] apply.
///
/// Pending replay entries are not drained; a later eligible executor may run
/// surviving or newly registered actions on the same live Line, but cannot
/// snapshot the removed action. Pending service is not a registration epoch.
///
/// Requires sleepable task context accepted by [`TaskBlockingContextIf::is_sleepable_context`],
/// even when no handler is in flight. Context misuse is reported as
/// [`IrqDescError::InvalidContext`] without removing the action.
///
/// Callers must not hold locks or resources that the IRQ handler may acquire
/// while waiting, otherwise the handler can block behind its own teardown.
///
/// Context is checked before action removal. Unlinking and execution drain
/// require no action-retirement allocation or RCU grace-period wait. The task
/// waits for admitted original claims and running replay to finish before returning the handler.
/// Surviving shared actions remain dispatchable during this wait; removal
/// masks and gates the whole Line only when it withdraws the last action.
/// Continued traffic can prolong the Line-wide drain.
pub fn try_free_irq_action(
    spec: impl Into<IrqSpec>,
    token: IrqActionToken,
) -> Result<Option<Handler>, IrqDescError> {
    try_free_irq_inner(spec.into(), Some(token))
}

fn try_free_irq_inner(
    spec: IrqSpec,
    token: Option<IrqActionToken>,
) -> Result<Option<Handler>, IrqDescError> {
    can_wait_for_irq_sync("free_irq")?;
    with_action_removal(spec, ManagementMode::Task, |virq, line| {
        let Some(retired) =
            withdraw_action_on_line(virq, line, token, false, apply_platform_delivery)?
        else {
            return Ok(None);
        };
        IrqDrain::new(virq, line).wait();
        // SAFETY: unlink and snapshot capture share the Line lock. Every old
        // traversal retains admission through original claim/replay completion;
        // the locked zero observation ended those borrows. New captures cannot
        // reach the detached node, and management excludes another writer.
        let removed = unsafe { retired.reclaim() };
        Ok(Some(removed.into_primary()))
    })
}

// Management admission pins membership through mask and the subsequent drain.
// Same-Line registration/removal is excluded by management; enable/disable
// serialize with withdrawal on the state lock. Last-action teardown rejects
// re-enable, while surviving actions retain their ordinary delivery policy.
// No global control exclusion is needed for ordinary action withdrawal.
// Return the removed owner: neither this short transaction nor its caller's
// management-locked body may run the client's final destructor.
fn withdraw_action_on_line(
    virq: Virq,
    line: &Arc<IrqLine>,
    token: Option<IrqActionToken>,
    is_percpu_only: bool,
    apply_plan: impl FnOnce(IrqPlatformPlan),
) -> Result<Option<RetiredAction>, IrqDescError> {
    let mut entry = line.lock();
    if is_percpu_only && entry.flow != Some(IrqFlow::PerCpu) {
        return Err(IrqDescError::UnsupportedFlow {
            hwirq: entry.desc.hwirq,
        });
    }
    if !is_percpu_only {
        require_non_percpu_flow(&entry)?;
    }
    if entry.has_enabled_local_cpu() {
        warn!("free_irq for IRQ {virq} rejected: disable the local source on every CPU first");
        return Ok(None);
    }
    if token.is_none() && entry.action_count() > 1 {
        warn!("free_irq for IRQ {virq} requires an action token");
        return Ok(None);
    }
    let removed_action = entry.withdraw_action(token);
    if removed_action.is_none() && token.is_none() && entry.action_count() == 1 {
        warn!(
            "free_irq for IRQ {virq} rejected: action belongs to shared or NMI registration; use \
             its owning removal API"
        );
    }
    // Absence is not a disable request: a legacy-enabled empty Line must keep
    // its control state when no action matched the caller's removal key.
    let Some(removed_action) = removed_action else {
        return Ok(None);
    };
    if !entry.has_actions() {
        entry.cancel_pending_affinity();
    }
    let disable_plan = entry.prepare_disable_if_no_actions();
    // New snapshots cannot observe withdrawal until the last-action mask has
    // completed. Ordinary old snapshots remain covered by Line drain; PerCpu
    // removal instead requires prior source-owner execution exclusion.
    if let Some(plan) = disable_plan {
        entry.begin_teardown();
        apply_plan(plan);
        entry.commit_platform_plan(plan);
    }
    Ok(Some(removed_action))
}

fn require_non_percpu_flow(entry: &IrqStateDesc) -> Result<(), IrqDescError> {
    require_bound_flow(entry)?;
    if entry.flow == Some(IrqFlow::PerCpu) {
        return Err(IrqDescError::UnsupportedFlow {
            hwirq: entry.desc.hwirq,
        });
    }
    Ok(())
}

fn require_bound_flow(entry: &IrqStateDesc) -> Result<(), IrqDescError> {
    entry.flow.map(|_| ()).ok_or(IrqDescError::UnsupportedFlow {
        hwirq: entry.desc.hwirq,
    })
}

unittest::path_mod_test!(tests_action_helpers, "../../ut-tests/action_helpers.rs");

unittest::path_mod_test!(pub mod tests_irq, "../../ut-tests/runtime_manager.rs");
unittest::path_mod_test!(
    tests_enable_preparation,
    "../../ut-tests/enable_preparation.rs"
);
unittest::path_mod_test!(
    tests_management_identity,
    "../../ut-tests/management_identity.rs"
);

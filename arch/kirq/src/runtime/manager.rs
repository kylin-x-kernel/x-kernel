// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IRQ manager public API and dispatch-entry orchestration.

use krcu_core::RcuContextIf;
use kspin::{SpinNoIrq, static_lock};

use super::{rcu::with_nmi_context, sync_wait::IrqSyncWaitIf};
use crate::{
    Hwirq, IrqDesc, IrqDescError, IrqDomainId, IrqSpec, Virq,
    action::IrqAction,
    context::HardIrqContextGuard,
    dispatch::dispatch_actions,
    lifecycle::IrqLifecycleGuard,
    nmi::dispatch_nmi_handler,
    platform::{
        configure_platform_irq, platform_dispatch_irq, platform_dispatch_nmi,
        set_platform_irq_enabled,
    },
    state::{IRQ_STATE, IrqPlatformPlan, IrqState, IrqStateDesc, try_resolve_and_publish},
};
pub use crate::{
    action::IrqActionToken,
    deferred::{
        DeferredExecutorHook, DeferredExecutorHooks, DeferredRunContext, DeferredRunResult,
        clear_deferred_executor, register_deferred_executor, run_hardirq_exit_deferred,
    },
    lifecycle::{
        IrqLifecycleHook, IrqLifecycleHooks, clear_irq_lifecycle_hooks,
        register_irq_lifecycle_hooks,
    },
    nmi::{register_nmi, unregister_nmi},
    platform::{
        DispatchedIrq, Handler, IntrManagerIf, PendingIrq, TargetCpu, notify_cpu, set_prio,
    },
    state::DYNAMIC_VIRQ_BASE,
};

static_lock! {
    static IRQ_CONTROL_LOCK: SpinNoIrq<()> = SpinNoIrq::new(());
}

/// Options for installing a regular IRQ handler.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IrqRegisterOptions {
    auto_enable: bool,
}

impl IrqRegisterOptions {
    /// Register and enable the IRQ line after successful handler installation.
    pub const fn auto_enable() -> Self {
        Self { auto_enable: true }
    }

    /// Register the handler but leave delivery disabled until explicit enable.
    pub const fn disabled() -> Self {
        Self { auto_enable: false }
    }

    /// Returns whether successful registration should enable the line.
    pub const fn should_auto_enable(self) -> bool {
        self.auto_enable
    }
}

impl Default for IrqRegisterOptions {
    fn default() -> Self {
        Self::auto_enable()
    }
}

fn apply_platform_plan(plan: IrqPlatformPlan) {
    if plan.configure {
        configure_platform_irq(plan.desc);
    }
    if let Some(on) = plan.enable {
        set_platform_irq_enabled(plan.desc, on);
    }
}
/// Try to map a hardware IRQ resource into the OS-visible logical IRQ namespace.
pub fn try_map(desc: IrqDesc) -> Result<Virq, IrqDescError> {
    let mut state = IRQ_STATE.lock();
    #[expect(
        clippy::unwrap_used,
        reason = "CG-021-no-panic-on-reachable-failure: try_resolve_and_publish returns only \
                  descriptors assigned a logical IRQ"
    )]
    Ok(try_resolve_and_publish(&mut state, IrqSpec::Desc(desc))?
        .logical_irq()
        .unwrap())
}

/// Returns the mapped logical IRQ number for a domain-local hardware IRQ.
///
/// This is a lock-free read from the domain's published reverse-map snapshot.
/// `None` means the domain has no mapping and no explicit identity policy.
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
        warn!("enable IRQ failed: {err:?}");
    }
}

/// Try to enable or disable an IRQ line through the legacy boolean bridge.
///
/// `on == true` preserves the legacy force-enable behavior for static and
/// per-CPU bring-up sites. `on == false` follows [`try_disable_irq_nosync`], so
/// it is valid from hardirq context and does not wait for in-flight handlers.
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
        warn!("enable IRQ failed: {err:?}");
    }
}

/// Try to enable an IRQ line after a previous disable or disabled registration.
pub fn try_enable_irq(spec: impl Into<IrqSpec>) -> Result<(), IrqDescError> {
    try_enable_irq_inner(spec.into(), false)
}

fn try_enable_irq_inner(spec: IrqSpec, force_platform_enable: bool) -> Result<(), IrqDescError> {
    let _control_guard = IRQ_CONTROL_LOCK.lock();
    let plan = {
        let mut state = IRQ_STATE.lock();
        if force_platform_enable {
            if let Some(virq) = state.lookup_virq(spec) {
                let _ = reject_teardown(
                    state.descs.get(&virq),
                    virq,
                    "enable IRQ",
                    TeardownReject::Error,
                )?;
            }
            let desc = try_resolve_and_publish(&mut state, spec)?;
            #[expect(
                clippy::unwrap_used,
                reason = "CG-021-no-panic-on-reachable-failure: try_resolve_and_publish succeeded \
                          and assigned this descriptor a logical IRQ"
            )]
            let virq = desc.logical_irq().unwrap();
            #[expect(
                clippy::expect_used,
                reason = "CG-021-no-panic-on-reachable-failure: try_resolve_and_publish installed \
                          this descriptor in the same exclusively borrowed registry"
            )]
            let entry = state
                .descs
                .get_mut(&virq)
                .expect("descriptor state must exist after try_resolve_desc");
            entry.prepare_legacy_enable()
        } else {
            let virq = state.lookup_virq(spec).ok_or(IrqDescError::UnknownIrq)?;
            let entry = state.descs.get_mut(&virq).ok_or(IrqDescError::UnknownIrq)?;
            let _ = reject_teardown(Some(entry), virq, "enable IRQ", TeardownReject::Error)?;
            if !entry.has_actions() {
                warn!("enable IRQ {virq} failed because no IRQ action is registered");
                return Err(IrqDescError::NoIrqAction { virq });
            }
            entry.prepare_enable_irq()
        }
    };
    apply_platform_plan(plan);
    Ok(())
}

/// Disable an IRQ line without waiting for an in-flight handler to finish.
pub fn disable_irq_nosync(spec: impl Into<IrqSpec>) {
    if let Err(err) = try_disable_irq_nosync(spec) {
        warn!("disable IRQ failed: {err:?}");
    }
}

/// Try to disable an existing IRQ line without waiting for an in-flight handler to finish.
///
/// This is a lookup-only control operation: it never creates descriptors,
/// allocates `virq`s, or publishes domain mappings. Unknown IRQs return
/// [`IrqDescError::UnknownIrq`].
pub fn try_disable_irq_nosync(spec: impl Into<IrqSpec>) -> Result<(), IrqDescError> {
    let spec = spec.into();
    let _control_guard = IRQ_CONTROL_LOCK.lock();
    let plan = {
        let mut state = IRQ_STATE.lock();
        let virq = state.lookup_virq(spec).ok_or(IrqDescError::UnknownIrq)?;
        let entry = state.descs.get_mut(&virq).ok_or(IrqDescError::UnknownIrq)?;
        entry.prepare_disable_irq_nosync()
    };
    apply_platform_plan(plan);
    Ok(())
}

/// Disable an IRQ line and wait for currently running handlers to finish.
///
/// This API must be called from ordinary kernel context. It returns `false`
/// when the descriptor cannot be resolved or the caller is already in an
/// interrupt-like context where IRQ synchronization would deadlock.
///
/// Callers must not hold locks or resources that the IRQ handler may acquire
/// while waiting, otherwise the handler can block behind its own teardown.
pub fn disable_irq(spec: impl Into<IrqSpec>) -> bool {
    match try_disable_irq(spec) {
        Ok(done) => done,
        Err(err) => {
            warn!("disable IRQ failed: {err:?}");
            false
        }
    }
}

/// Try to disable an IRQ line and wait for currently running handlers to finish.
///
/// The wait is only valid outside hardirq, softirq, and BH-disabled context.
/// Context misuse is reported as [`IrqDescError::InvalidContext`].
///
/// Callers must not hold locks or resources that the IRQ handler may acquire
/// while waiting, otherwise the handler can block behind its own teardown.
pub fn try_disable_irq(spec: impl Into<IrqSpec>) -> Result<bool, IrqDescError> {
    let spec = spec.into();
    can_wait_for_irq_sync("disable_irq")?;
    try_disable_irq_nosync(spec)?;
    try_synchronize_irq(spec)
}

/// Wait until the current software hardirq dispatch for an IRQ line has finished.
///
/// This API does not mask the line. Call [`disable_irq`] first when teardown
/// must prevent new handler entries before waiting for existing dispatches.
///
/// Callers must not hold locks or resources that the IRQ handler may acquire
/// while waiting, otherwise the handler can block behind its own teardown.
pub fn synchronize_irq(spec: impl Into<IrqSpec>) -> bool {
    match try_synchronize_irq(spec) {
        Ok(done) => done,
        Err(err) => {
            warn!("synchronize IRQ failed: {err:?}");
            false
        }
    }
}

/// Try to wait until the current software hardirq dispatch for an IRQ line has finished.
///
/// The wait is only valid outside hardirq, softirq, and BH-disabled context.
/// Context misuse is reported as [`IrqDescError::InvalidContext`].
///
/// Callers must not hold locks or resources that the IRQ handler may acquire
/// while waiting, otherwise the handler can block behind its own teardown.
pub fn try_synchronize_irq(spec: impl Into<IrqSpec>) -> Result<bool, IrqDescError> {
    can_wait_for_irq_sync("synchronize_irq")?;
    let virq = {
        let state = IRQ_STATE.lock();
        state.lookup_virq(spec.into())
    };
    if let Some(virq) = virq {
        wait_for_virq_in_flight(virq, "synchronize_irq")?;
        return Ok(true);
    }
    Ok(false)
}

fn can_wait_for_irq_sync(api: &'static str) -> Result<(), IrqDescError> {
    if crate::context::is_in_interrupt_context() {
        warn!("{api} called from non-sleepable IRQ context");
        return Err(IrqDescError::InvalidContext { operation: api });
    }
    Ok(())
}

fn wait_for_virq_in_flight(virq: Virq, operation: &'static str) -> Result<(), IrqDescError> {
    loop {
        let (in_flight, completion) = {
            let state = IRQ_STATE.lock();
            let Some(entry) = state.descs.get(&virq) else {
                return Ok(());
            };
            let in_flight = entry.in_flight();
            if in_flight == 0 {
                return Ok(());
            }
            (in_flight, entry.in_flight_zero_completion())
        };
        observe_wait_for_in_flight_for_tests(virq, in_flight);
        IrqSyncWaitIf::wait_for_completion(&completion)
            .map_err(|error| IrqDescError::SyncWaitFailed { operation, error })?;
    }
}

#[cfg(unittest)]
type WaitForInFlightHook = fn(Virq, usize);

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
static WAIT_FOR_IN_FLIGHT_HOOK: SpinNoIrq<Option<WaitForInFlightHook>> = SpinNoIrq::new(None);

#[cfg(unittest)]
fn observe_wait_for_in_flight_for_tests(virq: Virq, in_flight: usize) {
    let hook = *WAIT_FOR_IN_FLIGHT_HOOK.lock();
    if let Some(hook) = hook {
        hook(virq, in_flight);
    }
}

#[cfg(not(unittest))]
#[inline(always)]
fn observe_wait_for_in_flight_for_tests(_virq: Virq, _in_flight: usize) {}

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
pub fn register(spec: impl Into<IrqSpec>, handler: Handler) -> bool {
    match try_register(spec, handler) {
        Ok(registered) => registered,
        Err(err) => {
            warn!("register IRQ handler failed: {err:?}");
            false
        }
    }
}

/// Try to register the regular OS IRQ handler for an IRQ line.
pub fn try_register(spec: impl Into<IrqSpec>, handler: Handler) -> Result<bool, IrqDescError> {
    try_register_with_options(spec, handler, IrqRegisterOptions::auto_enable())
}

/// Register the regular OS IRQ handler without automatically enabling delivery.
pub fn register_disabled(spec: impl Into<IrqSpec>, handler: Handler) -> bool {
    match try_register_disabled(spec, handler) {
        Ok(registered) => registered,
        Err(err) => {
            warn!("register disabled IRQ handler failed: {err:?}");
            false
        }
    }
}

/// Try to register the regular OS IRQ handler without automatically enabling delivery.
pub fn try_register_disabled(
    spec: impl Into<IrqSpec>,
    handler: Handler,
) -> Result<bool, IrqDescError> {
    try_register_with_options(spec, handler, IrqRegisterOptions::disabled())
}

/// Try to register the regular OS IRQ handler with explicit lifecycle options.
pub fn try_register_with_options(
    spec: impl Into<IrqSpec>,
    handler: Handler,
    options: IrqRegisterOptions,
) -> Result<bool, IrqDescError> {
    let spec = spec.into();
    let _control_guard = IRQ_CONTROL_LOCK.lock();
    let mut state = IRQ_STATE.lock();
    let Some((_desc, virq, entry)) =
        resolve_action_install_entry(&mut state, spec, "register handler")?
    else {
        return Ok(false);
    };
    if entry.has_actions() {
        warn!("register handler for IRQ {virq} failed");
        return Ok(false);
    }
    let installed = entry.install_regular_action(IrqAction::regular(handler));
    debug_assert!(installed, "duplicate regular action was checked above");
    let plan = if options.should_auto_enable() {
        entry.prepare_auto_enable()
    } else {
        entry.prepare_register_disabled()
    };
    drop(state);
    apply_platform_plan(plan);
    Ok(true)
}

/// Register one shared IRQ action and return its removal token.
///
/// All actions already installed on the line must be shared actions. The first
/// shared registration configures/enables the line with default auto-enable
/// behavior; later shared registrations only add another action to the
/// core-owned fanout list.
pub fn register_shared(spec: impl Into<IrqSpec>, handler: Handler) -> Option<IrqActionToken> {
    match try_register_shared(spec, handler) {
        Ok(token) => token,
        Err(err) => {
            warn!("register shared IRQ handler failed: {err:?}");
            None
        }
    }
}

/// Try to register one shared IRQ action with default auto-enable behavior.
pub fn try_register_shared(
    spec: impl Into<IrqSpec>,
    handler: Handler,
) -> Result<Option<IrqActionToken>, IrqDescError> {
    let spec = spec.into();
    let _control_guard = IRQ_CONTROL_LOCK.lock();
    let mut state = IRQ_STATE.lock();
    let Some((_desc, virq, entry)) =
        resolve_action_install_entry(&mut state, spec, "register shared handler")?
    else {
        return Ok(None);
    };
    let was_first_action = !entry.has_actions();
    let Some(token) = entry.install_shared_action(handler) else {
        warn!("register shared handler for IRQ {virq} failed");
        return Ok(None);
    };
    let plan = if was_first_action {
        entry.prepare_auto_enable()
    } else {
        entry.prepare_reconfigure_if_stale()
    };
    drop(state);
    apply_platform_plan(plan);
    Ok(Some(token))
}

fn resolve_action_install_entry<'a>(
    state: &'a mut IrqState,
    spec: IrqSpec,
    operation: &'static str,
) -> Result<Option<(IrqDesc, Virq, &'a mut IrqStateDesc)>, IrqDescError> {
    if let Some(virq) = state.lookup_virq(spec)
        && reject_teardown(
            state.descs.get(&virq),
            virq,
            operation,
            TeardownReject::ReturnNone,
        )?
    {
        return Ok(None);
    }
    let desc = try_resolve_and_publish(state, spec)?;
    #[expect(
        clippy::unwrap_used,
        reason = "CG-021-no-panic-on-reachable-failure: try_resolve_and_publish succeeded and \
                  assigned this descriptor a logical IRQ"
    )]
    let virq = desc.logical_irq().unwrap();
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: try_resolve_and_publish installed this \
                  descriptor in the same exclusively borrowed registry"
    )]
    let entry = state
        .descs
        .get_mut(&virq)
        .expect("descriptor state must exist after try_resolve_desc");
    Ok(Some((desc, virq, entry)))
}

enum TeardownReject {
    Error,
    ReturnNone,
}

fn reject_teardown(
    entry: Option<&IrqStateDesc>,
    virq: Virq,
    operation: &'static str,
    reject: TeardownReject,
) -> Result<bool, IrqDescError> {
    if entry.is_some_and(IrqStateDesc::is_teardown_in_progress) {
        warn!("{operation} for IRQ {virq} failed while teardown is in progress");
        if matches!(reject, TeardownReject::Error) {
            return Err(IrqDescError::TeardownInProgress { virq });
        }
        return Ok(true);
    }
    Ok(false)
}

/// Remove the regular OS IRQ handler for an IRQ line.
///
/// This is a compatibility wrapper for [`free_irq`].
///
/// # Panics
///
/// Inherits [`free_irq`]'s fail-stop behavior if teardown synchronization fails
/// after the action has already been removed from the dispatch table.
pub fn unregister(spec: impl Into<IrqSpec>) -> Option<Handler> {
    free_irq(spec)
}

/// Remove the regular OS IRQ handler and wait for in-flight dispatch to finish.
///
/// The line is masked once the last regular action is removed. The API
/// returns after any handler snapshot that was already running has exited, so
/// device teardown can safely release MMIO or DMA state owned by the handler.
///
/// Callers must not hold locks or resources that the IRQ handler may acquire
/// while waiting, otherwise the handler can block behind its own teardown.
///
/// # Panics
///
/// Panics if the action has been removed but the scheduler wait provider cannot
/// wait for an already escaped dispatch snapshot to exit. At that point the IRQ
/// core cannot report a clean retryable failure without exposing a half-removed
/// action lifecycle to the caller.
pub fn free_irq(spec: impl Into<IrqSpec>) -> Option<Handler> {
    match try_free_irq(spec) {
        Ok(handler) => handler,
        Err(err) => {
            warn!("free IRQ handler failed: {err:?}");
            None
        }
    }
}

/// Try to remove the regular OS IRQ handler and synchronize dispatch teardown.
///
/// The wait is only valid outside hardirq, softirq, and BH-disabled context.
/// Context misuse is reported as [`IrqDescError::InvalidContext`].
///
/// Callers must not hold locks or resources that the IRQ handler may acquire
/// while waiting, otherwise the handler can block behind its own teardown.
///
/// # Panics
///
/// Panics if teardown synchronization fails after removing the action. Errors
/// reported before action removal, such as invalid interrupt context, are
/// returned as [`IrqDescError`].
pub fn try_free_irq(spec: impl Into<IrqSpec>) -> Result<Option<Handler>, IrqDescError> {
    try_free_irq_inner(spec.into(), None)
}

/// Remove one shared IRQ action by token and synchronize dispatch teardown.
///
/// Callers must not hold locks or resources that the IRQ handler may acquire
/// while waiting, otherwise the handler can block behind its own teardown.
///
/// # Panics
///
/// Panics if the action has been removed but teardown synchronization cannot
/// wait for already escaped dispatch snapshots to exit.
pub fn free_irq_action(spec: impl Into<IrqSpec>, token: IrqActionToken) -> Option<Handler> {
    match try_free_irq_action(spec, token) {
        Ok(handler) => handler,
        Err(err) => {
            warn!("free shared IRQ handler failed: {err:?}");
            None
        }
    }
}

/// Try to remove one shared IRQ action by token and synchronize dispatch teardown.
///
/// The wait is only valid outside hardirq, softirq, and BH-disabled context.
/// Context misuse is reported as [`IrqDescError::InvalidContext`].
///
/// Callers must not hold locks or resources that the IRQ handler may acquire
/// while waiting, otherwise the handler can block behind its own teardown.
///
/// # Panics
///
/// Panics if teardown synchronization fails after removing the action. Errors
/// reported before action removal, such as invalid interrupt context, are
/// returned as [`IrqDescError`].
pub fn try_free_irq_action(
    spec: impl Into<IrqSpec>,
    token: IrqActionToken,
) -> Result<Option<Handler>, IrqDescError> {
    try_free_irq_inner(spec.into(), Some(token))
}

#[expect(
    clippy::panic,
    reason = "CG-021-no-panic-on-reachable-failure: TODO(cg): waiter registration can fail (e.g. \
              NoMemory) after the action is removed and teardown has begun; preallocate and \
              register the completion waiter before removing the action so the failure returns an \
              error with the action still installed"
)]
fn try_free_irq_inner(
    spec: IrqSpec,
    token: Option<IrqActionToken>,
) -> Result<Option<Handler>, IrqDescError> {
    can_wait_for_irq_sync("free_irq")?;
    let (virq, removed_action) = {
        let _control_guard = IRQ_CONTROL_LOCK.lock();
        let mut state = IRQ_STATE.lock();
        let Some(virq) = state.lookup_virq(spec) else {
            return Ok(None);
        };
        let Some(entry) = state.descs.get_mut(&virq) else {
            return Ok(None);
        };
        let removed_action = if let Some(token) = token {
            entry.take_action(token)
        } else {
            if entry.action_count() > 1 {
                warn!("free_irq for IRQ {virq} requires an action token");
                return Ok(None);
            }
            let removed_action = entry.take_regular_action();
            if removed_action.is_none() && entry.action_count() == 1 {
                warn!(
                    "free_irq for IRQ {virq} rejected: the only action is shared; use \
                     free_irq_action with its token"
                );
            }
            removed_action
        };
        // Removing the action first prevents new dispatch snapshots from seeing
        // it. If this was the last action, mask the platform line before waiting
        // for snapshots that already escaped under an `IrqDispatchGuard`.
        let disable_plan = entry.prepare_disable_if_no_actions();
        if removed_action.is_some() {
            entry.begin_teardown();
        }
        drop(state);
        if let Some(plan) = disable_plan {
            apply_platform_plan(plan);
        }
        (virq, removed_action)
    };

    if removed_action.is_none() {
        return Ok(None);
    }

    if let Err(err) = wait_for_virq_in_flight(virq, "free_irq") {
        panic!("free_irq for IRQ {virq} failed to synchronize after action removal: {err:?}");
    }
    finish_irq_teardown(virq);

    Ok(removed_action.map(IrqAction::into_primary))
}

fn finish_irq_teardown(virq: Virq) {
    let _control_guard = IRQ_CONTROL_LOCK.lock();
    let mut state = IRQ_STATE.lock();
    if let Some(entry) = state.descs.get_mut(&virq) {
        entry.finish_teardown();
    }
    state.remove_if_unused_with_desc(virq);
}

/// Handles a pseudo-NMI after an architecture trap adapter has disabled
/// preemption.
///
/// Invoked from the exception entry layer when `dispatch_exception` detects
/// that normal IRQs were masked (PMR <= NMI_ONLY) at the moment the interrupt
/// fired — a pseudo-NMI preempting a critical section. The handler uses the
/// lock-free NMI path which only touches the NMI table, never `IRQ_STATE`.
pub fn handle_nmi(vector: usize) -> bool {
    handle_nmi_with(vector, platform_dispatch_nmi)
}

fn handle_nmi_with(vector: usize, dispatch_fn: impl FnOnce(usize)) -> bool {
    // SAFETY: the architecture adapter excludes ordinary IRQs and migration;
    // controller dispatch completes all claims before returning to this stack.
    unsafe { with_nmi_context(|| dispatch_fn(vector)) };
    true
}

/// Dispatches and completes a claimed normal IRQ.
///
/// This is the generic entry for an irqchip that has classified its claim as a
/// normal IRQ. It resolves the claim, runs regular action fanout, and completes
/// the controller claim. Its return value is only the resolved normal `virq`
/// metadata used by the outer IRQ-exit deferred hook.
///
/// The caller must be the active [`IntrManagerIf::dispatch_irq`] invocation on
/// the CPU that created `claim`, with local IRQs masked and generic hardirq
/// context already entered.
#[doc(hidden)]
pub fn generic_handle_irq(claim: PendingIrq) -> Option<Virq> {
    let resolved_irq = dispatch_actions(&claim);
    claim.complete();
    resolved_irq
}

/// Dispatches and completes a claimed NMI without touching normal IRQ state.
///
/// This is the generic entry for an irqchip that classifies an IRQ-on claim as
/// NMI-class. Dispatch is keyed by the raw hwirq stored in `claim`; it bypasses
/// domain translation and normal IRQ action fanout. [`handle_nmi`] also uses
/// this helper for an interrupt that entered through the architecture's
/// dedicated NMI path.
///
/// The claim must be consumed on the CPU that acknowledged it.
#[doc(hidden)]
pub fn generic_handle_nmi(claim: DispatchedIrq) {
    // SAFETY: the active controller claim belongs to this CPU in an IRQ/NMI
    // entry. Neither handler dispatch nor claim completion may schedule.
    unsafe {
        with_nmi_context(|| {
            // Keep effective NMI context through controller completion, even
            // when classification happened inside an ordinary IRQ entry.
            dispatch_nmi_handler(claim.irq());
            claim.complete();
        })
    };
}

/// Handles a normal IRQ after an architecture trap adapter has masked local
/// IRQs and disabled preemption.
///
/// Invoked when the interrupted context had normal IRQs enabled (PMR >
/// NMI_ONLY). The caller must keep the current CPU pinned and local IRQs
/// masked for the duration of this call. No critical-section locks are held, so
/// it is safe to acquire [`IRQ_STATE`].
///
/// # Contract
///
/// Callers must be in a hardirq trap path or an equivalent hypervisor VM-exit
/// path and must not hold locks that can be taken by driver IRQ handlers.
/// `handle_irq()` itself is non-sleepable.
///
/// The irqchip classifies each successful claim and calls either
/// [`generic_handle_irq`] or [`generic_handle_nmi`]. For a claimed normal IRQ,
/// the generic ordering is:
///
/// 1. establish RCU watching, then enter lifecycle hooks and hardirq context;
/// 2. claim, ack, and classify through [`IntrManagerIf::dispatch_irq`];
/// 3. invoke the irqchip-selected generic IRQ or NMI helper;
/// 4. complete the controller claim inside that helper;
/// 5. leave generic hardirq context;
/// 6. run the common IRQ-exit deferred hook;
/// 7. leave lifecycle hooks, then finish the watching entry before returning
///    to the HAL scheduling tail.
///
/// As on Linux, an NMI discovered after entering from an IRQ-on context remains
/// inside the outer IRQ lifecycle. Only its inner handler dispatch differs:
/// it uses the lock-free NMI table rather than normal IRQ action state.
pub fn handle_irq(vector: usize) -> bool {
    handle_irq_with(vector, platform_dispatch_irq)
}

fn handle_irq_with(vector: usize, dispatch_fn: impl FnOnce(usize) -> Option<Virq>) -> bool {
    // SAFETY: the HAL adapter masks ordinary IRQs and pins this CPU through
    // dispatch and deferred work. End watching entry before its scheduling tail.
    let interrupt_entry = unsafe { RcuContextIf::current_cpu_watching().enter_interrupt() };
    let lifecycle_guard = IrqLifecycleGuard::enter();
    let hardirq_context_guard = HardIrqContextGuard::enter();

    // The irqchip consumes every successful claim through one of the generic
    // helpers before returning. What remains here is the common outer
    // IRQ-entry tail, including IRQ-on NMI and spurious-claim cases.
    let resolved_irq = dispatch_fn(vector);
    run_irq_exit_tail_with_context(vector, resolved_irq, hardirq_context_guard);

    drop(lifecycle_guard);
    // SAFETY: controller, nested entries, deferred work and lifecycle hooks
    // have finished without scheduling; the HAL exclusion is still held.
    unsafe { interrupt_entry.exit() };
    true
}

unittest::path_mod_test!(tests_rcu_entry, "../../ut-tests/rcu_irq_entry.rs");

#[cfg(unittest)]
fn run_claimed_irq_tail(
    vector: usize,
    pending_irq: PendingIrq,
    complete_irq: impl FnOnce(PendingIrq),
) {
    let hardirq_context_guard = HardIrqContextGuard::enter();
    let resolved_irq = dispatch_actions(&pending_irq);
    complete_irq(pending_irq);
    run_irq_exit_tail_with_context(vector, resolved_irq, hardirq_context_guard);
}

fn run_irq_exit_tail_with_context(
    vector: usize,
    resolved_irq: Option<Virq>,
    hardirq_context_guard: HardIrqContextGuard,
) {
    drop(hardirq_context_guard);
    crate::deferred::run_hardirq_exit_deferred(crate::deferred::DeferredRunContext::new(
        vector,
        resolved_irq,
    ));
}

unittest::path_mod_test!(pub mod tests_irq, "../../ut-tests/runtime_manager.rs");

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IRQ manager public API and dispatch-entry orchestration.

use kspin::SpinNoIrq;

use super::sync_wait::IrqSyncWaitIf;
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

static IRQ_CONTROL_LOCK: SpinNoIrq<()> = SpinNoIrq::new(());

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
            let virq = desc.logical_irq().unwrap();
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
/// This is different from wakeup subscription: the registered handler is invoked
/// directly on dispatch, while wakeup subscribers only participate in the wakeup
/// notification path.
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
    let virq = desc.logical_irq().unwrap();
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
    let removed_desc = {
        let _control_guard = IRQ_CONTROL_LOCK.lock();
        let mut state = IRQ_STATE.lock();
        if let Some(entry) = state.descs.get_mut(&virq) {
            entry.finish_teardown();
        }
        state.remove_if_unused_with_desc(virq)
    };
    if removed_desc.is_some() {
        super::notify::remove_irq_waiters(virq);
    }
}

/// Handles a pseudo-NMI after an architecture trap adapter has disabled
/// preemption.
///
/// Invoked from the exception entry layer when `dispatch_exception` detects
/// that normal IRQs were masked (PMR <= NMI_ONLY) at the moment the interrupt
/// fired — a pseudo-NMI preempting a critical section. The handler uses the
/// lock-free NMI path which only touches the NMI table, never `IRQ_STATE`.
pub fn handle_nmi(vector: usize) -> bool {
    if let Some(dispatched_irq) = platform_dispatch_nmi(vector) {
        // The NMI backend returns a raw hwirq in the claimed IRQ object. Do not
        // translate it through IRQ_STATE here; NMI dispatch is keyed by hwirq.
        dispatch_nmi_handler(dispatched_irq.irq());
        dispatched_irq.complete();
    }

    true
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
/// For a claimed normal IRQ, the generic ordering is:
///
/// 1. enter lifecycle hooks;
/// 2. enter generic hardirq context;
/// 3. claim and ack through [`IntrManagerIf::dispatch_irq`];
/// 4. run regular action fanout outside `IRQ_STATE`;
/// 5. complete the controller claim through [`PendingIrq::complete`];
/// 6. leave generic hardirq context;
/// 7. run hardirq-exit deferred execution;
/// 8. leave lifecycle hooks.
///
/// Spurious vectors that cannot be claimed skip handler fanout and deferred
/// execution, but still preserve lifecycle enter/exit pairing.
pub fn handle_irq(vector: usize) -> bool {
    let lifecycle_guard = IrqLifecycleGuard::enter();
    let hardirq_context_guard = HardIrqContextGuard::enter();

    if let Some(pending_irq) = platform_dispatch_irq(vector) {
        run_claimed_irq_tail_with_context(
            vector,
            pending_irq,
            PendingIrq::complete,
            hardirq_context_guard,
        );
    } else {
        drop(hardirq_context_guard);
    }

    let _ = lifecycle_guard;
    true
}

#[cfg(unittest)]
fn run_claimed_irq_tail(
    vector: usize,
    pending_irq: PendingIrq,
    complete_irq: impl FnOnce(PendingIrq),
) {
    let hardirq_context_guard = HardIrqContextGuard::enter();
    run_claimed_irq_tail_with_context(vector, pending_irq, complete_irq, hardirq_context_guard);
}

fn run_claimed_irq_tail_with_context(
    vector: usize,
    pending_irq: PendingIrq,
    complete_irq: impl FnOnce(PendingIrq),
    hardirq_context_guard: HardIrqContextGuard,
) {
    let resolved_irq = dispatch_actions(&pending_irq);
    complete_irq(pending_irq);
    drop(hardirq_context_guard);
    crate::deferred::run_hardirq_exit_deferred(crate::deferred::DeferredRunContext::new(
        vector,
        resolved_irq,
    ));
}

#[cfg(unittest)]
#[allow(missing_docs)]
pub mod tests_irq {
    use alloc::{boxed::Box, sync::Arc, vec::Vec};
    use core::{
        sync::atomic::{AtomicUsize, Ordering},
        task::{Context, RawWaker, RawWakerVTable, Waker},
    };

    use kpoll::PollRegistrations;
    use unittest::def_test;

    use super::{
        DYNAMIC_VIRQ_BASE, IRQ_STATE, IrqDesc, IrqDescError, IrqDomainId, IrqStateDesc,
        WAIT_FOR_IN_FLIGHT_HOOK, dispatch_actions, run_claimed_irq_tail, try_disable_irq,
        try_disable_irq_nosync, try_enable, try_enable_irq, try_free_irq, try_free_irq_action,
        try_map, try_register, try_register_disabled, try_register_shared, try_synchronize_irq,
        unregister,
    };
    use crate::{
        GIC_ROOT_DOMAIN, IrqController, IrqEvent, IrqFlags, IrqLifecycleHooks, IrqRef, IrqSource,
        IrqTrigger, MSI_DOMAIN, PendingIrq,
        action::{IrqAction, IrqActionFlags, IrqThreadSlot},
        clear_irq_lifecycle_hooks,
        lifecycle::IrqLifecycleGuard,
        register_irq_lifecycle_hooks, register_irq_source_waker, register_irq_waker,
        runtime::notify,
        softirq::{
            SoftirqRunResult, SoftirqVec, local_softirq_pending, open_softirq, raise_softirq,
            reset_softirq_for_tests, run_pending_softirqs, softirq_diagnostics,
        },
        state::MAX_IRQ_ACTIONS,
    };

    static IRQ_LIFECYCLE_TEST_LOCK: kspin::SpinNoIrq<()> = kspin::SpinNoIrq::new(());
    static REGULAR_CALLS: AtomicUsize = AtomicUsize::new(0);
    static SHARED_A_CALLS: AtomicUsize = AtomicUsize::new(0);
    static SHARED_B_CALLS: AtomicUsize = AtomicUsize::new(0);
    static HANDLER_OBSERVED_IRQ: AtomicUsize = AtomicUsize::new(0);
    static IRQ_ENTER_CALLS: AtomicUsize = AtomicUsize::new(0);
    static IRQ_EXIT_CALLS: AtomicUsize = AtomicUsize::new(0);
    static IRQ_TAIL_ORDER_SEQ: AtomicUsize = AtomicUsize::new(1);
    static IRQ_TAIL_HANDLER_ORDER: AtomicUsize = AtomicUsize::new(0);
    static IRQ_TAIL_COMPLETE_ORDER: AtomicUsize = AtomicUsize::new(0);
    static IRQ_TAIL_DEFERRED_ORDER: AtomicUsize = AtomicUsize::new(0);
    static IRQ_TAIL_LIFECYCLE_EXIT_ORDER: AtomicUsize = AtomicUsize::new(0);
    static IRQ_TAIL_DEFERRED_IN_HARDIRQ: AtomicUsize = AtomicUsize::new(0);
    static IRQ_TAIL_TEST_ACTIVE: AtomicUsize = AtomicUsize::new(0);
    static IRQ_TAIL_SOFTIRQ_ACTION_ORDER: AtomicUsize = AtomicUsize::new(0);
    static IRQ_TAIL_SOFTIRQ_IN_HARDIRQ: AtomicUsize = AtomicUsize::new(0);
    static IRQ_TAIL_SOFTIRQ_SERVING: AtomicUsize = AtomicUsize::new(0);
    static IRQ_TAIL_UNRESOLVED_DEFERRED: AtomicUsize = AtomicUsize::new(0);
    static IRQ_IN_FLIGHT_OBSERVED: AtomicUsize = AtomicUsize::new(0);
    static IRQ_WAIT_TEST_TARGET: AtomicUsize = AtomicUsize::new(0);
    static IRQ_WAIT_TEST_OBSERVED: AtomicUsize = AtomicUsize::new(0);
    static IRQ_WAIT_TEST_RELEASES: AtomicUsize = AtomicUsize::new(0);

    const IRQ_TAIL_TEST_VECTOR: usize = 0xdead;
    const IRQ_TAIL_TEST_VIRQ: usize = 0x2f02;
    const IRQ_TAIL_SOFTIRQ_VIRQ: usize = 0x2f03;
    const IRQ_SIMULATED_TEST_VECTOR: usize = 0x5f00;
    const IRQ_SIMULATED_TEST_VIRQ: usize = 0x5f01;
    const IRQ_LIFECYCLE_TEST_VECTOR: usize = 0x5f10;
    const IRQ_LIFECYCLE_TEST_VIRQ: usize = 0x5f11;
    const IRQ_IN_FLIGHT_TEST_VIRQ: usize = 0x5f21;
    const MSI_TEST_HWIRQ_BASE: usize = 0x30_000;
    const MSI_TEST_VIRQ_BASE: usize = 0x40_000;

    fn test_handler(_irq: usize) -> IrqEvent {
        REGULAR_CALLS.fetch_add(1, Ordering::Relaxed);
        IrqEvent::HANDLED
    }

    fn test_not_handled_handler(_irq: usize) -> IrqEvent {
        REGULAR_CALLS.fetch_add(1, Ordering::Relaxed);
        IrqEvent::NOT_HANDLED
    }

    fn test_shared_a_handler(_irq: usize) -> IrqEvent {
        SHARED_A_CALLS.fetch_add(1, Ordering::Relaxed);
        IrqEvent::from_sources(0b0000_0001)
    }

    fn test_shared_b_handler(_irq: usize) -> IrqEvent {
        SHARED_B_CALLS.fetch_add(1, Ordering::Relaxed);
        IrqEvent::from_sources(0b0000_0010)
    }

    fn test_observe_irq_handler(irq: usize) -> IrqEvent {
        HANDLER_OBSERVED_IRQ.store(irq, Ordering::Relaxed);
        IrqEvent::HANDLED
    }

    fn test_in_flight_handler(_irq: usize) -> IrqEvent {
        let in_flight = IRQ_STATE
            .lock()
            .descs
            .get(&IRQ_IN_FLIGHT_TEST_VIRQ)
            .map_or(0, IrqStateDesc::in_flight_for_tests);
        IRQ_IN_FLIGHT_OBSERVED.store(in_flight, Ordering::Relaxed);
        IrqEvent::HANDLED
    }

    unsafe fn clone_waker(data: *const ()) -> RawWaker {
        RawWaker::new(data, &WAKER_VTABLE)
    }

    unsafe fn wake_waker(data: *const ()) {
        // SAFETY: `make_waker` installs a leaked, aligned `AtomicUsize` pointer.
        let counter = unsafe { &*(data as *const AtomicUsize) };
        counter.fetch_add(1, Ordering::SeqCst);
    }

    unsafe fn wake_waker_by_ref(data: *const ()) {
        // SAFETY: same invariant as `wake_waker`.
        unsafe { wake_waker(data) };
    }

    unsafe fn drop_waker(_data: *const ()) {}

    static WAKER_VTABLE: RawWakerVTable =
        RawWakerVTable::new(clone_waker, wake_waker, wake_waker_by_ref, drop_waker);

    fn make_waker(counter: &'static AtomicUsize) -> Waker {
        let raw = RawWaker::new(counter as *const _ as *const (), &WAKER_VTABLE);
        // SAFETY: all vtable operations preserve the leaked AtomicUsize pointer.
        unsafe { Waker::from_raw(raw) }
    }

    fn test_release_in_flight_on_wait(virq: usize, in_flight: usize) {
        if IRQ_WAIT_TEST_TARGET.load(Ordering::Acquire) != virq {
            return;
        }
        let _ = IRQ_WAIT_TEST_OBSERVED.compare_exchange(
            0,
            in_flight,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        if IRQ_WAIT_TEST_RELEASES.fetch_add(1, Ordering::AcqRel) == 0 {
            let wait_set = IRQ_STATE
                .lock()
                .descs
                .get_mut(&virq)
                .and_then(IrqStateDesc::finish_dispatch);
            if let Some(wait_set) = wait_set {
                wait_set.wake();
            }
        }
    }

    fn test_irq_enter_hook() {
        IRQ_ENTER_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    fn test_irq_exit_hook() {
        IRQ_EXIT_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    fn record_irq_tail_order(slot: &AtomicUsize) {
        if slot.load(Ordering::Relaxed) == 0 {
            let order = IRQ_TAIL_ORDER_SEQ.fetch_add(1, Ordering::Relaxed);
            let _ = slot.compare_exchange(0, order, Ordering::Relaxed, Ordering::Relaxed);
        }
    }

    fn test_irq_tail_handler(_irq: usize) -> IrqEvent {
        record_irq_tail_order(&IRQ_TAIL_HANDLER_ORDER);
        IrqEvent::HANDLED
    }

    fn test_irq_tail_complete() {
        record_irq_tail_order(&IRQ_TAIL_COMPLETE_ORDER);
    }

    fn test_irq_tail_deferred_hook(ctx: crate::deferred::DeferredRunContext) {
        if ctx.vector() == IRQ_TAIL_TEST_VECTOR && ctx.resolved_irq() == Some(IRQ_TAIL_TEST_VIRQ) {
            IRQ_TAIL_DEFERRED_IN_HARDIRQ.store(
                usize::from(crate::context::is_in_hardirq()),
                Ordering::Relaxed,
            );
            record_irq_tail_order(&IRQ_TAIL_DEFERRED_ORDER);
            IRQ_TAIL_TEST_ACTIVE.store(1, Ordering::Release);
        }
    }

    fn test_irq_tail_unresolved_deferred_hook(ctx: crate::deferred::DeferredRunContext) {
        if ctx.vector() == IRQ_TAIL_TEST_VECTOR && ctx.resolved_irq().is_none() {
            IRQ_TAIL_UNRESOLVED_DEFERRED.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn test_irq_tail_lifecycle_exit_hook() {
        if IRQ_TAIL_TEST_ACTIVE
            .compare_exchange(1, 2, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            record_irq_tail_order(&IRQ_TAIL_LIFECYCLE_EXIT_ORDER);
        }
    }

    fn test_irq_tail_raise_softirq_handler(_irq: usize) -> IrqEvent {
        record_irq_tail_order(&IRQ_TAIL_HANDLER_ORDER);
        raise_softirq(SoftirqVec::High);
        IrqEvent::HANDLED
    }

    fn test_irq_tail_softirq_action() {
        IRQ_TAIL_SOFTIRQ_IN_HARDIRQ.store(
            usize::from(crate::context::is_in_hardirq()),
            Ordering::Relaxed,
        );
        IRQ_TAIL_SOFTIRQ_SERVING.store(
            usize::from(crate::context::is_serving_softirq()),
            Ordering::Relaxed,
        );
        record_irq_tail_order(&IRQ_TAIL_SOFTIRQ_ACTION_ORDER);
    }

    fn unused_msi_test_mapping(state: &crate::runtime::state::IrqState) -> Option<(usize, usize)> {
        (0..1024).find_map(|offset| {
            let hwirq = MSI_TEST_HWIRQ_BASE + offset;
            let virq = MSI_TEST_VIRQ_BASE + offset;
            (unused_msi_test_hwirq(state, hwirq) && !state.descs.contains_key(&virq))
                .then_some((hwirq, virq))
        })
    }

    fn unused_msi_test_hwirq(state: &crate::runtime::state::IrqState, hwirq: usize) -> bool {
        state.translated_hwirq(MSI_DOMAIN, hwirq).is_none()
    }

    fn unused_msi_test_hwirq_except(
        state: &crate::runtime::state::IrqState,
        excluded_hwirq: usize,
    ) -> Option<usize> {
        (0..1024)
            .map(|offset| MSI_TEST_HWIRQ_BASE + offset)
            .find(|&hwirq| hwirq != excluded_hwirq && unused_msi_test_hwirq(state, hwirq))
    }

    #[def_test(serial)]
    fn test_simulated_irq_tail_dispatches_regular_handler() {
        let _irq_guard = kspin::NoPreemptIrqSave::new();
        REGULAR_CALLS.store(0, Ordering::Relaxed);
        crate::deferred::clear_deferred_executor();

        {
            let mut state = IRQ_STATE.lock();
            state.descs.insert(
                IRQ_SIMULATED_TEST_VIRQ,
                IrqStateDesc::test_with_runtime(
                    IrqDesc::from_virq(IRQ_SIMULATED_TEST_VIRQ),
                    Some(IrqAction::regular(Arc::new(test_handler))),
                ),
            );
        }

        let pending = PendingIrq::new(
            IrqRef::Virq(IRQ_SIMULATED_TEST_VIRQ),
            IRQ_SIMULATED_TEST_VIRQ,
        );
        run_claimed_irq_tail(IRQ_SIMULATED_TEST_VECTOR, pending, |pending| {
            pending.complete();
        });

        crate::deferred::clear_deferred_executor();
        {
            let mut state = IRQ_STATE.lock();
            state.descs.remove(&IRQ_SIMULATED_TEST_VIRQ);
        }

        assert_eq!(REGULAR_CALLS.load(Ordering::Relaxed), 1);
    }

    #[def_test(serial)]
    fn test_try_map_rejects_conflicting_domain_mapping() {
        let domain = GIC_ROOT_DOMAIN;
        let hwirq = 0x2f11;
        let first = IrqDesc::new(hwirq, IrqTrigger::EdgeRising).with_domain(domain);
        let virq = try_map(first).expect("initial map should succeed");
        let conflicting_virq = virq + 1;
        let conflict = IrqDesc::new(hwirq, IrqTrigger::EdgeRising)
            .with_domain(domain)
            .with_virq(conflicting_virq);

        let err = try_map(conflict).expect_err("conflicting map must fail");

        assert!(matches!(
            err,
            IrqDescError::MappingConflict {
                domain: got_domain,
                hwirq: got_hwirq,
                existing,
                newer,
            } if got_domain == domain
                && got_hwirq == hwirq
                && existing == virq
                && newer == conflicting_virq
        ));
        assert!(!IRQ_STATE.lock().descs.contains_key(&conflicting_virq));
        let _ = IRQ_STATE.lock().remove_if_unused(virq);
    }

    #[def_test(serial)]
    fn test_try_map_rolls_back_domain_mapping_on_desc_conflict() {
        let domain = GIC_ROOT_DOMAIN;
        let hwirq = 0x2f21;
        let virq = 0x2f22;
        {
            let mut state = IRQ_STATE.lock();
            state.descs.insert(
                virq,
                IrqStateDesc::new(IrqDesc::new(0x2f23, IrqTrigger::EdgeRising).with_virq(virq)),
            );
        }

        let conflict = IrqDesc::new(hwirq, IrqTrigger::EdgeRising)
            .with_domain(domain)
            .with_virq(virq);
        let err = try_map(conflict).expect_err("desc conflict must fail");

        assert!(matches!(err, IrqDescError::HwirqConflict { .. }));
        assert_eq!(IRQ_STATE.lock().translated_hwirq(domain, hwirq), None);
        IRQ_STATE.lock().descs.remove(&virq);
    }

    #[def_test(serial)]
    fn test_try_map_rejects_reusing_virq_for_another_domain_mapping() {
        let domain = GIC_ROOT_DOMAIN;
        let existing_hwirq = 0x2f2a;
        let newer_hwirq = 0x2f2b;
        let virq =
            try_map(IrqDesc::new(existing_hwirq, IrqTrigger::EdgeRising).with_domain(domain))
                .expect("initial map should succeed");
        IRQ_STATE.lock().descs.remove(&virq);

        let err = try_map(
            IrqDesc::new(newer_hwirq, IrqTrigger::EdgeRising)
                .with_domain(domain)
                .with_virq(virq),
        )
        .expect_err("virq already targeted by another mapping must fail");

        assert!(matches!(
            err,
            IrqDescError::VirqMappingConflict {
                virq: got_virq,
                existing_domain,
                existing_hwirq: got_existing_hwirq,
                newer_domain,
                newer_hwirq: got_newer_hwirq,
            } if got_virq == virq
                && existing_domain == domain
                && got_existing_hwirq == existing_hwirq
                && newer_domain == domain
                && got_newer_hwirq == newer_hwirq
        ));
        assert_eq!(IRQ_STATE.lock().translated_hwirq(domain, newer_hwirq), None);
    }

    #[def_test(serial)]
    fn test_try_map_rejects_dynamic_virq_exhaustion_without_wrap() {
        let domain = GIC_ROOT_DOMAIN;
        let hwirq = 0x2f25;
        {
            let mut state = IRQ_STATE.lock();
            state.set_next_virq_for_tests(usize::MAX);
        }

        let err = try_map(IrqDesc::new(hwirq, IrqTrigger::EdgeRising).with_domain(domain))
            .expect_err("dynamic virq exhaustion must fail");

        assert!(matches!(
            err,
            IrqDescError::VirqExhausted { next } if next == usize::MAX
        ));
        {
            let mut state = IRQ_STATE.lock();
            assert_eq!(state.next_virq_for_tests(), usize::MAX);
            assert_eq!(state.translated_hwirq(domain, hwirq), None);
            state.set_next_virq_for_tests(DYNAMIC_VIRQ_BASE);
        }
    }

    #[def_test(serial)]
    fn test_try_map_rejects_unknown_domain_without_state_change() {
        let domain = IrqDomainId::new(0x2f26);
        let hwirq = 0x2f27;

        let err = try_map(IrqDesc::new(hwirq, IrqTrigger::EdgeRising).with_domain(domain))
            .expect_err("unknown irq domain must fail");

        assert!(matches!(
            err,
            IrqDescError::UnknownDomain { domain: got } if got == domain
        ));
        assert_eq!(IRQ_STATE.lock().translated_hwirq(domain, hwirq), None);
    }

    #[def_test]
    fn test_descriptor_update_bumps_generation_on_metadata_change() {
        let desc = IrqDesc::from_virq(0x2f2c);
        let mut state_desc = IrqStateDesc::new(desc);
        let initial_generation = state_desc.generation_for_tests();

        state_desc.update_desc(desc);
        assert_eq!(state_desc.generation_for_tests(), initial_generation);

        state_desc.update_desc(desc.with_flags(IrqFlags::MSI));
        assert!(state_desc.generation_for_tests() > initial_generation);
    }

    #[def_test]
    fn test_teardown_gate_blocks_new_actions_until_finished() {
        let desc = IrqDesc::from_virq(0x2f2e);
        let mut state_desc = IrqStateDesc::new(desc);

        state_desc.begin_teardown();
        assert_eq!(state_desc.teardown_depth_for_tests(), 1);
        assert!(!state_desc.install_regular_action(IrqAction::regular(Arc::new(test_handler))));
        assert!(
            state_desc
                .install_shared_action(Arc::new(test_shared_a_handler))
                .is_none()
        );

        state_desc.finish_teardown();
        assert_eq!(state_desc.teardown_depth_for_tests(), 0);
        assert!(state_desc.install_regular_action(IrqAction::regular(Arc::new(test_handler))));
    }

    #[def_test]
    fn test_teardown_disable_plan_ignores_existing_teardown_waiter() {
        let desc = IrqDesc::from_virq(0x2f2f);
        let mut state_desc = IrqStateDesc::new(desc);
        let token_a = state_desc
            .install_shared_action(Arc::new(test_shared_a_handler))
            .expect("first shared action");
        let token_b = state_desc
            .install_shared_action(Arc::new(test_shared_b_handler))
            .expect("second shared action");
        let _ = state_desc.prepare_auto_enable();

        assert!(state_desc.take_action(token_a).is_some());
        assert!(state_desc.prepare_disable_if_no_actions().is_none());
        state_desc.begin_teardown();
        assert!(state_desc.take_action(token_b).is_some());

        let plan = state_desc
            .prepare_disable_if_no_actions()
            .expect("last action should mask line even during teardown wait");
        assert_eq!(plan.enable, Some(false));
        assert!(!state_desc.is_enabled_for_tests());
        assert_eq!(state_desc.teardown_depth_for_tests(), 1);
    }

    #[def_test(serial)]
    fn test_irq_lifecycle_hooks_fire_around_normal_irq_handler() {
        let _test_guard = IRQ_LIFECYCLE_TEST_LOCK.lock();
        let _irq_guard = kspin::NoPreemptIrqSave::new();

        clear_irq_lifecycle_hooks();
        crate::deferred::clear_deferred_executor();
        let enter_before = IRQ_ENTER_CALLS.load(Ordering::Relaxed);
        let exit_before = IRQ_EXIT_CALLS.load(Ordering::Relaxed);
        {
            let mut state = IRQ_STATE.lock();
            state.descs.insert(
                IRQ_LIFECYCLE_TEST_VIRQ,
                IrqStateDesc::test_with_runtime(
                    IrqDesc::from_virq(IRQ_LIFECYCLE_TEST_VIRQ),
                    Some(IrqAction::regular(Arc::new(test_handler))),
                ),
            );
        }

        assert!(register_irq_lifecycle_hooks(IrqLifecycleHooks {
            on_irq_enter: Some(test_irq_enter_hook),
            on_irq_exit: Some(test_irq_exit_hook),
        }));

        {
            let lifecycle_guard = IrqLifecycleGuard::enter();
            let pending = PendingIrq::new(
                IrqRef::Virq(IRQ_LIFECYCLE_TEST_VIRQ),
                IRQ_LIFECYCLE_TEST_VIRQ,
            );
            run_claimed_irq_tail(IRQ_LIFECYCLE_TEST_VECTOR, pending, |pending| {
                pending.complete();
            });
            drop(lifecycle_guard);
        }

        crate::deferred::clear_deferred_executor();
        clear_irq_lifecycle_hooks();
        {
            let mut state = IRQ_STATE.lock();
            state.descs.remove(&IRQ_LIFECYCLE_TEST_VIRQ);
        }

        assert!(IRQ_ENTER_CALLS.load(Ordering::Relaxed) > enter_before);
        assert!(IRQ_EXIT_CALLS.load(Ordering::Relaxed) > exit_before);
    }

    #[def_test(serial)]
    fn test_irq_lifecycle_guard_uses_exit_snapshot() {
        let _test_guard = IRQ_LIFECYCLE_TEST_LOCK.lock();
        clear_irq_lifecycle_hooks();
        let exit_before = IRQ_EXIT_CALLS.load(Ordering::Relaxed);

        assert!(register_irq_lifecycle_hooks(IrqLifecycleHooks {
            on_irq_enter: None,
            on_irq_exit: Some(test_irq_exit_hook),
        }));

        let lifecycle_guard = IrqLifecycleGuard::enter();
        clear_irq_lifecycle_hooks();
        drop(lifecycle_guard);

        assert!(IRQ_EXIT_CALLS.load(Ordering::Relaxed) > exit_before);
    }

    #[def_test(serial)]
    fn test_claimed_irq_tail_orders_completion_deferred_and_lifecycle_exit() {
        let _test_guard = IRQ_LIFECYCLE_TEST_LOCK.lock();
        let _irq_guard = kspin::NoPreemptIrqSave::new();

        IRQ_TAIL_ORDER_SEQ.store(1, Ordering::Relaxed);
        IRQ_TAIL_HANDLER_ORDER.store(0, Ordering::Relaxed);
        IRQ_TAIL_COMPLETE_ORDER.store(0, Ordering::Relaxed);
        IRQ_TAIL_DEFERRED_ORDER.store(0, Ordering::Relaxed);
        IRQ_TAIL_LIFECYCLE_EXIT_ORDER.store(0, Ordering::Relaxed);
        IRQ_TAIL_DEFERRED_IN_HARDIRQ.store(0, Ordering::Relaxed);
        IRQ_TAIL_TEST_ACTIVE.store(0, Ordering::Relaxed);

        crate::deferred::clear_deferred_executor();
        clear_irq_lifecycle_hooks();
        {
            let mut state = IRQ_STATE.lock();
            state.descs.insert(
                IRQ_TAIL_TEST_VIRQ,
                IrqStateDesc::test_with_runtime(
                    IrqDesc::from_virq(IRQ_TAIL_TEST_VIRQ),
                    Some(IrqAction::regular(Arc::new(test_irq_tail_handler))),
                ),
            );
        }
        assert!(crate::deferred::register_deferred_executor(
            crate::deferred::DeferredExecutorHooks {
                on_hardirq_exit: Some(test_irq_tail_deferred_hook),
            }
        ));
        assert!(register_irq_lifecycle_hooks(IrqLifecycleHooks {
            on_irq_enter: None,
            on_irq_exit: Some(test_irq_tail_lifecycle_exit_hook),
        }));

        {
            let lifecycle_guard = IrqLifecycleGuard::enter();
            let pending = PendingIrq::new(IrqRef::Virq(IRQ_TAIL_TEST_VIRQ), IRQ_TAIL_TEST_VIRQ);
            run_claimed_irq_tail(IRQ_TAIL_TEST_VECTOR, pending, |pending| {
                test_irq_tail_complete();
                pending.complete();
            });
            drop(lifecycle_guard);
        }

        crate::deferred::clear_deferred_executor();
        clear_irq_lifecycle_hooks();
        IRQ_TAIL_TEST_ACTIVE.store(0, Ordering::Relaxed);
        {
            let mut state = IRQ_STATE.lock();
            state.descs.remove(&IRQ_TAIL_TEST_VIRQ);
        }

        let handler_order = IRQ_TAIL_HANDLER_ORDER.load(Ordering::Relaxed);
        let complete_order = IRQ_TAIL_COMPLETE_ORDER.load(Ordering::Relaxed);
        let deferred_order = IRQ_TAIL_DEFERRED_ORDER.load(Ordering::Relaxed);
        let lifecycle_exit_order = IRQ_TAIL_LIFECYCLE_EXIT_ORDER.load(Ordering::Relaxed);
        assert!(handler_order != 0);
        assert!(handler_order < complete_order);
        assert!(complete_order < deferred_order);
        assert!(deferred_order < lifecycle_exit_order);
        assert_eq!(IRQ_TAIL_DEFERRED_IN_HARDIRQ.load(Ordering::Relaxed), 0);
    }

    #[def_test(serial)]
    fn test_claimed_irq_tail_runs_deferred_for_unresolved_strict_domain() {
        let _test_guard = IRQ_LIFECYCLE_TEST_LOCK.lock();
        let _irq_guard = kspin::NoPreemptIrqSave::new();

        IRQ_TAIL_COMPLETE_ORDER.store(0, Ordering::Relaxed);
        IRQ_TAIL_UNRESOLVED_DEFERRED.store(0, Ordering::Relaxed);
        crate::deferred::clear_deferred_executor();

        assert!(crate::deferred::register_deferred_executor(
            crate::deferred::DeferredExecutorHooks {
                on_hardirq_exit: Some(test_irq_tail_unresolved_deferred_hook),
            }
        ));

        let unresolved_hwirq = {
            let state = IRQ_STATE.lock();
            unused_msi_test_mapping(&state)
                .expect("MSI test mapping space exhausted")
                .0
        };
        let pending = PendingIrq::new(IrqRef::Domain(MSI_DOMAIN, unresolved_hwirq), 0);
        run_claimed_irq_tail(IRQ_TAIL_TEST_VECTOR, pending, |pending| {
            test_irq_tail_complete();
            pending.complete();
        });

        crate::deferred::clear_deferred_executor();
        assert_ne!(IRQ_TAIL_COMPLETE_ORDER.load(Ordering::Relaxed), 0);
        assert_eq!(IRQ_TAIL_UNRESOLVED_DEFERRED.load(Ordering::Relaxed), 1);
    }

    #[def_test(serial)]
    fn test_softirq_runs_from_claimed_irq_tail_after_hardirq_exit() {
        let _test_guard = IRQ_LIFECYCLE_TEST_LOCK.lock();
        let _irq_guard = kspin::NoPreemptIrqSave::new();

        IRQ_TAIL_ORDER_SEQ.store(1, Ordering::Relaxed);
        IRQ_TAIL_HANDLER_ORDER.store(0, Ordering::Relaxed);
        IRQ_TAIL_COMPLETE_ORDER.store(0, Ordering::Relaxed);
        IRQ_TAIL_SOFTIRQ_ACTION_ORDER.store(0, Ordering::Relaxed);
        IRQ_TAIL_SOFTIRQ_IN_HARDIRQ.store(1, Ordering::Relaxed);
        IRQ_TAIL_SOFTIRQ_SERVING.store(0, Ordering::Relaxed);

        crate::deferred::clear_deferred_executor();
        reset_softirq_for_tests();
        {
            let mut state = IRQ_STATE.lock();
            state.descs.insert(
                IRQ_TAIL_SOFTIRQ_VIRQ,
                IrqStateDesc::test_with_runtime(
                    IrqDesc::from_virq(IRQ_TAIL_SOFTIRQ_VIRQ),
                    Some(IrqAction::regular(Arc::new(
                        test_irq_tail_raise_softirq_handler,
                    ))),
                ),
            );
        }

        assert!(open_softirq(SoftirqVec::High, test_irq_tail_softirq_action));
        assert!(crate::softirq::init());

        let pending = PendingIrq::new(IrqRef::Virq(IRQ_TAIL_SOFTIRQ_VIRQ), IRQ_TAIL_SOFTIRQ_VIRQ);
        run_claimed_irq_tail(IRQ_TAIL_TEST_VECTOR, pending, |pending| {
            test_irq_tail_complete();
            pending.complete();
        });

        let handler_order = IRQ_TAIL_HANDLER_ORDER.load(Ordering::Relaxed);
        let complete_order = IRQ_TAIL_COMPLETE_ORDER.load(Ordering::Relaxed);
        let softirq_order = IRQ_TAIL_SOFTIRQ_ACTION_ORDER.load(Ordering::Relaxed);
        let softirq_in_hardirq = IRQ_TAIL_SOFTIRQ_IN_HARDIRQ.load(Ordering::Relaxed);
        let softirq_serving = IRQ_TAIL_SOFTIRQ_SERVING.load(Ordering::Relaxed);
        let pending = local_softirq_pending();
        let runs = softirq_diagnostics().runs;
        let drain_again = run_pending_softirqs();

        crate::deferred::clear_deferred_executor();
        reset_softirq_for_tests();
        {
            let mut state = IRQ_STATE.lock();
            state.descs.remove(&IRQ_TAIL_SOFTIRQ_VIRQ);
        }

        assert!(handler_order != 0);
        assert!(handler_order < complete_order);
        assert!(complete_order < softirq_order);
        assert_eq!(softirq_in_hardirq, 0);
        assert_eq!(softirq_serving, 1);
        assert_eq!(pending, 0);
        assert_eq!(runs, 1);
        assert_eq!(drain_again, SoftirqRunResult::NoPending);
    }

    #[def_test(serial)]
    fn test_duplicate_regular_handler_registration_is_rejected() {
        let virq = 0x3101;
        REGULAR_CALLS.store(0, Ordering::Relaxed);

        assert!(try_register(virq, Arc::new(test_handler)).expect("first register should work"));
        assert!(
            !try_register(virq, Arc::new(test_not_handled_handler))
                .expect("duplicate register should report compatibility false")
        );

        let pending = PendingIrq::new(IrqRef::Virq(virq), virq);
        dispatch_actions(&pending);

        assert_eq!(REGULAR_CALLS.load(Ordering::Relaxed), 1);
        assert!(unregister(virq).is_some());
    }

    #[def_test(serial)]
    fn test_shared_irq_fanout_runs_all_actions() {
        let virq = 0x3120;
        SHARED_A_CALLS.store(0, Ordering::Relaxed);
        SHARED_B_CALLS.store(0, Ordering::Relaxed);

        let token_a = try_register_shared(virq, Arc::new(test_shared_a_handler))
            .expect("first shared register should work")
            .expect("first shared action token");
        let token_b = try_register_shared(virq, Arc::new(test_shared_b_handler))
            .expect("second shared register should work")
            .expect("second shared action token");

        let pending = PendingIrq::new(IrqRef::Virq(virq), virq);
        dispatch_actions(&pending);

        assert_eq!(SHARED_A_CALLS.load(Ordering::Relaxed), 1);
        assert_eq!(SHARED_B_CALLS.load(Ordering::Relaxed), 1);
        assert!(try_free_irq_action(virq, token_a).unwrap().is_some());
        assert!(try_free_irq_action(virq, token_b).unwrap().is_some());
    }

    #[def_test(serial)]
    fn test_dispatch_wakes_kirq_owned_line_and_source_waiters() {
        let virq = 0x3130;
        let line_counter = Box::leak(Box::new(AtomicUsize::new(0)));
        let source_counter = Box::leak(Box::new(AtomicUsize::new(0)));
        let line_waker = make_waker(line_counter);
        let source_waker = make_waker(source_counter);
        let line_cx = Context::from_waker(&line_waker);
        let source_cx = Context::from_waker(&source_waker);
        let mut line_registrations = PollRegistrations::new();
        let mut source_registrations = PollRegistrations::new();

        {
            let mut context = line_registrations.context(&line_cx);
            register_irq_waker(virq, &mut context).unwrap();
        }
        {
            let mut context = source_registrations.context(&source_cx);
            register_irq_source_waker(virq, 1, &mut context).unwrap();
        }
        let token = try_register_shared(virq, Arc::new(test_shared_b_handler))
            .expect("shared register should work")
            .expect("shared action token");

        let pending = PendingIrq::new(IrqRef::Virq(virq), virq);
        dispatch_actions(&pending);

        assert_eq!(line_counter.load(Ordering::SeqCst), 1);
        assert_eq!(source_counter.load(Ordering::SeqCst), 1);
        assert!(try_free_irq_action(virq, token).unwrap().is_some());
        assert!(!notify::has_irq_waiters_for_tests(virq));
    }

    #[def_test(serial)]
    fn test_irq_waiters_are_removed_with_unused_descriptor() {
        let virq = 0x3132;
        let wake_counter = Box::leak(Box::new(AtomicUsize::new(0)));
        let waker = make_waker(wake_counter);
        let cx = Context::from_waker(&waker);
        let mut registrations = PollRegistrations::new();
        let baseline_entries = notify::waiter_entry_count_for_tests();

        {
            let mut context = registrations.context(&cx);
            register_irq_waker(virq, &mut context).unwrap();
        }
        assert!(notify::has_irq_waiters_for_tests(virq));
        assert_eq!(notify::waiter_entry_count_for_tests(), baseline_entries + 1);

        assert!(try_register(virq, Arc::new(test_handler)).expect("register should work"));
        assert!(unregister(virq).is_some());

        assert!(!notify::has_irq_waiters_for_tests(virq));
        assert_eq!(notify::waiter_entry_count_for_tests(), baseline_entries);
        assert_eq!(wake_counter.load(Ordering::SeqCst), 1);
    }

    #[def_test(serial)]
    fn test_shared_irq_free_one_action_keeps_line_active() {
        let virq = 0x3121;
        SHARED_A_CALLS.store(0, Ordering::Relaxed);
        SHARED_B_CALLS.store(0, Ordering::Relaxed);

        let token_a = try_register_shared(virq, Arc::new(test_shared_a_handler))
            .expect("first shared register should work")
            .expect("first shared action token");
        let token_b = try_register_shared(virq, Arc::new(test_shared_b_handler))
            .expect("second shared register should work")
            .expect("second shared action token");
        assert!(try_free_irq_action(virq, token_a).unwrap().is_some());
        {
            let state = IRQ_STATE.lock();
            let entry = state.descs.get(&virq).expect("descriptor must remain");
            assert_eq!(entry.action_count(), 1);
            assert!(entry.is_enabled_for_tests());
        }

        let pending = PendingIrq::new(IrqRef::Virq(virq), virq);
        dispatch_actions(&pending);

        assert_eq!(SHARED_A_CALLS.load(Ordering::Relaxed), 0);
        assert_eq!(SHARED_B_CALLS.load(Ordering::Relaxed), 1);
        assert!(try_free_irq_action(virq, token_b).unwrap().is_some());
        assert!(!IRQ_STATE.lock().descs.contains_key(&virq));
    }

    #[def_test(serial)]
    fn test_shared_irq_action_limit_is_enforced() {
        let virq = 0x312c;
        SHARED_A_CALLS.store(0, Ordering::Relaxed);

        let mut tokens = Vec::new();
        for _ in 0..MAX_IRQ_ACTIONS {
            let token = try_register_shared(virq, Arc::new(test_shared_a_handler))
                .expect("shared register should not fail")
                .expect("shared action within limit should install");
            tokens.push(token);
        }

        assert!(
            try_register_shared(virq, Arc::new(test_shared_a_handler))
                .expect("over-limit shared register is a policy rejection")
                .is_none()
        );
        {
            let state = IRQ_STATE.lock();
            let entry = state.descs.get(&virq).expect("descriptor must remain");
            assert_eq!(entry.action_count(), MAX_IRQ_ACTIONS);
        }

        let pending = PendingIrq::new(IrqRef::Virq(virq), virq);
        dispatch_actions(&pending);
        assert_eq!(SHARED_A_CALLS.load(Ordering::Relaxed), MAX_IRQ_ACTIONS);

        for token in tokens {
            assert!(try_free_irq_action(virq, token).unwrap().is_some());
        }
        assert!(!IRQ_STATE.lock().descs.contains_key(&virq));
    }

    #[def_test(serial)]
    fn test_free_irq_rejects_single_shared_action_without_token() {
        let virq = 0x312d;

        let token = try_register_shared(virq, Arc::new(test_shared_a_handler))
            .expect("shared register should work")
            .expect("shared action token");

        assert!(
            try_free_irq(virq)
                .expect("shared-only rejection is not a descriptor error")
                .is_none()
        );
        {
            let state = IRQ_STATE.lock();
            let entry = state.descs.get(&virq).expect("descriptor must remain");
            assert_eq!(entry.action_count(), 1);
        }

        assert!(try_free_irq_action(virq, token).unwrap().is_some());
    }

    #[def_test(serial)]
    fn test_free_irq_action_rejects_forged_regular_token() {
        let virq = 0x3125;

        assert!(try_register(virq, Arc::new(test_handler)).expect("regular register should work"));
        assert!(
            try_free_irq_action(virq, crate::IrqActionToken::new(0))
                .expect("forged token rejection is not a descriptor error")
                .is_none()
        );
        {
            let state = IRQ_STATE.lock();
            let entry = state.descs.get(&virq).expect("descriptor must remain");
            assert_eq!(entry.action_count(), 1);
        }

        assert!(unregister(virq).is_some());
    }

    #[def_test(serial)]
    fn test_regular_and_shared_registration_do_not_mix() {
        let virq = 0x3123;

        assert!(try_register(virq, Arc::new(test_handler)).expect("regular register should work"));
        assert!(
            try_register_shared(virq, Arc::new(test_shared_a_handler))
                .expect("mixed shared request is a policy rejection")
                .is_none()
        );
        assert!(unregister(virq).is_some());
    }

    #[def_test(serial)]
    fn test_register_rejected_during_teardown_has_no_descriptor_side_effect() {
        let virq = 0x3126;
        let hwirq = 0x4326;
        let initial_desc = IrqDesc::new(hwirq, IrqTrigger::Unknown(0)).with_virq(virq);
        let richer_desc = initial_desc
            .with_controller(IrqController::Gic)
            .with_domain(GIC_ROOT_DOMAIN)
            .with_source(IrqSource::Acpi);

        {
            let mut state = IRQ_STATE.lock();
            state.descs.insert(virq, IrqStateDesc::new(initial_desc));
            state
                .descs
                .get_mut(&virq)
                .expect("descriptor must exist")
                .begin_teardown();
        }

        assert!(
            !try_register(richer_desc, Arc::new(test_handler))
                .expect("teardown rejection is not a descriptor error")
        );

        {
            let state = IRQ_STATE.lock();
            let entry = state.descs.get(&virq).expect("descriptor must remain");
            assert_eq!(entry.desc, initial_desc);
            assert_eq!(state.translated_hwirq(GIC_ROOT_DOMAIN, hwirq), None);
        }

        {
            let mut state = IRQ_STATE.lock();
            let entry = state.descs.get_mut(&virq).expect("descriptor must exist");
            entry.finish_teardown();
            let _ = state.remove_if_unused(virq);
        }
    }

    #[def_test(serial)]
    fn test_shared_register_rejected_during_teardown_has_no_descriptor_side_effect() {
        let virq = 0x312b;
        let hwirq = 0x432b;
        let initial_desc = IrqDesc::new(hwirq, IrqTrigger::Unknown(0)).with_virq(virq);
        let richer_desc = initial_desc
            .with_controller(IrqController::Gic)
            .with_domain(GIC_ROOT_DOMAIN)
            .with_source(IrqSource::Acpi);

        {
            let mut state = IRQ_STATE.lock();
            state.descs.insert(virq, IrqStateDesc::new(initial_desc));
            state
                .descs
                .get_mut(&virq)
                .expect("descriptor must exist")
                .begin_teardown();
        }

        assert!(
            try_register_shared(richer_desc, Arc::new(test_shared_a_handler))
                .expect("teardown rejection is not a descriptor error")
                .is_none()
        );

        {
            let state = IRQ_STATE.lock();
            let entry = state.descs.get(&virq).expect("descriptor must remain");
            assert_eq!(entry.desc, initial_desc);
            assert_eq!(entry.action_count(), 0);
            assert_eq!(state.translated_hwirq(GIC_ROOT_DOMAIN, hwirq), None);
        }

        {
            let mut state = IRQ_STATE.lock();
            let entry = state.descs.get_mut(&virq).expect("descriptor must exist");
            entry.finish_teardown();
            let _ = state.remove_if_unused(virq);
        }
    }

    #[def_test(serial)]
    fn test_try_register_auto_enable_marks_runtime_enabled() {
        let virq = 0x3110;

        assert!(try_register(virq, Arc::new(test_handler)).expect("register should work"));
        {
            let state = IRQ_STATE.lock();
            let entry = state.descs.get(&virq).expect("descriptor must exist");
            assert_eq!(entry.disable_depth_for_tests(), 0);
            assert!(entry.is_enabled_for_tests());
            assert_eq!(
                entry.configured_generation_for_tests(),
                Some(entry.generation_for_tests())
            );
        }

        assert!(unregister(virq).is_some());
    }

    #[def_test(serial)]
    fn test_try_register_disabled_leaves_runtime_disabled() {
        let virq = 0x3111;

        assert!(
            try_register_disabled(virq, Arc::new(test_handler))
                .expect("disabled register should work")
        );
        {
            let state = IRQ_STATE.lock();
            let entry = state.descs.get(&virq).expect("descriptor must exist");
            assert_eq!(entry.disable_depth_for_tests(), 1);
            assert!(!entry.is_enabled_for_tests());
            assert_eq!(entry.configured_generation_for_tests(), None);
        }

        assert!(unregister(virq).is_some());
    }

    #[def_test(serial)]
    fn test_enable_irq_starts_disabled_registration() {
        let virq = 0x3112;

        assert!(
            try_register_disabled(virq, Arc::new(test_handler))
                .expect("disabled register should work")
        );
        try_enable_irq(virq).expect("explicit enable should work");
        {
            let state = IRQ_STATE.lock();
            let entry = state.descs.get(&virq).expect("descriptor must exist");
            assert_eq!(entry.disable_depth_for_tests(), 0);
            assert!(entry.is_enabled_for_tests());
            assert_eq!(
                entry.configured_generation_for_tests(),
                Some(entry.generation_for_tests())
            );
        }

        assert!(unregister(virq).is_some());
    }

    #[def_test(serial)]
    fn test_enable_irq_rejects_teardown_wait_window() {
        let virq = 0x3127;
        let desc = IrqDesc::from_virq(virq);
        {
            let mut state = IRQ_STATE.lock();
            let mut entry = IrqStateDesc::new(desc);
            assert!(entry.install_regular_action(IrqAction::regular(Arc::new(test_handler))));
            let _ = entry.prepare_auto_enable();
            assert!(entry.take_regular_action().is_some());
            let _ = entry.prepare_disable_if_no_actions();
            entry.begin_teardown();
            state.descs.insert(virq, entry);
        }

        let err = try_enable_irq(virq).expect_err("teardown enable must fail");
        assert!(matches!(
            err,
            IrqDescError::TeardownInProgress { virq: got } if got == virq
        ));
        {
            let state = IRQ_STATE.lock();
            let entry = state.descs.get(&virq).expect("descriptor must remain");
            assert!(!entry.is_enabled_for_tests());
            assert_eq!(entry.teardown_depth_for_tests(), 1);
        }

        {
            let mut state = IRQ_STATE.lock();
            let entry = state.descs.get_mut(&virq).expect("descriptor must exist");
            entry.finish_teardown();
            let _ = state.remove_if_unused(virq);
        }
    }

    #[def_test(serial)]
    fn test_legacy_enable_rejected_during_teardown_has_no_descriptor_side_effect() {
        let virq = 0x312a;
        let hwirq = 0x432a;
        let initial_desc = IrqDesc::new(hwirq, IrqTrigger::Unknown(0)).with_virq(virq);
        let richer_desc = initial_desc
            .with_controller(IrqController::Gic)
            .with_domain(GIC_ROOT_DOMAIN)
            .with_source(IrqSource::Acpi);

        {
            let mut state = IRQ_STATE.lock();
            let mut entry = IrqStateDesc::new(initial_desc);
            entry.begin_teardown();
            state.descs.insert(virq, entry);
        }

        let err = try_enable(richer_desc, true).expect_err("teardown legacy enable must fail");
        assert!(matches!(
            err,
            IrqDescError::TeardownInProgress { virq: got } if got == virq
        ));
        {
            let state = IRQ_STATE.lock();
            let entry = state.descs.get(&virq).expect("descriptor must remain");
            assert_eq!(entry.desc, initial_desc);
            assert_eq!(state.translated_hwirq(GIC_ROOT_DOMAIN, hwirq), None);
        }

        {
            let mut state = IRQ_STATE.lock();
            let entry = state.descs.get_mut(&virq).expect("descriptor must exist");
            entry.finish_teardown();
            let _ = state.remove_if_unused(virq);
        }
    }

    #[def_test(serial)]
    fn test_enable_irq_rejects_descriptor_without_action() {
        let virq = 0x3128;
        {
            let mut state = IRQ_STATE.lock();
            state
                .descs
                .insert(virq, IrqStateDesc::new(IrqDesc::from_virq(virq)));
        }

        let err = try_enable_irq(virq).expect_err("action-less enable must fail");
        assert!(matches!(
            err,
            IrqDescError::NoIrqAction { virq: got } if got == virq
        ));
        {
            let state = IRQ_STATE.lock();
            let entry = state.descs.get(&virq).expect("descriptor must remain");
            assert!(!entry.is_enabled_for_tests());
            assert_eq!(entry.disable_depth_for_tests(), 0);
        }

        let _ = IRQ_STATE.lock().remove_if_unused(virq);
    }

    #[def_test(serial)]
    fn test_enable_irq_rejects_unknown_virq_without_creating_descriptor() {
        let virq = 0x3129;

        let err = try_enable_irq(virq).expect_err("unknown enable must fail");

        assert!(matches!(err, IrqDescError::UnknownIrq));
        assert!(!IRQ_STATE.lock().descs.contains_key(&virq));
    }

    #[def_test(serial)]
    fn test_disable_irq_nosync_uses_nested_depth() {
        let virq = 0x3113;

        assert!(try_register(virq, Arc::new(test_handler)).expect("register should work"));
        try_disable_irq_nosync(virq).expect("first disable should work");
        try_disable_irq_nosync(virq).expect("nested disable should work");
        {
            let state = IRQ_STATE.lock();
            let entry = state.descs.get(&virq).expect("descriptor must exist");
            assert_eq!(entry.disable_depth_for_tests(), 2);
            assert!(!entry.is_enabled_for_tests());
        }

        try_enable_irq(virq).expect("first enable should work");
        {
            let state = IRQ_STATE.lock();
            let entry = state.descs.get(&virq).expect("descriptor must exist");
            assert_eq!(entry.disable_depth_for_tests(), 1);
            assert!(!entry.is_enabled_for_tests());
        }

        try_enable_irq(virq).expect("second enable should work");
        {
            let state = IRQ_STATE.lock();
            let entry = state.descs.get(&virq).expect("descriptor must exist");
            assert_eq!(entry.disable_depth_for_tests(), 0);
            assert!(entry.is_enabled_for_tests());
        }

        assert!(unregister(virq).is_some());
    }

    #[def_test(serial)]
    fn test_disable_irq_nosync_rejects_unknown_virq_without_creating_descriptor() {
        let virq = 0x3119;

        assert!(!IRQ_STATE.lock().descs.contains_key(&virq));
        let err = try_disable_irq_nosync(virq).expect_err("unknown virq must fail");

        assert!(matches!(err, IrqDescError::UnknownIrq));
        assert!(!IRQ_STATE.lock().descs.contains_key(&virq));
    }

    #[def_test(serial)]
    fn test_dispatch_tracks_line_level_in_flight() {
        IRQ_IN_FLIGHT_OBSERVED.store(0, Ordering::Relaxed);
        {
            let mut state = IRQ_STATE.lock();
            state.descs.insert(
                IRQ_IN_FLIGHT_TEST_VIRQ,
                IrqStateDesc::test_with_runtime(
                    IrqDesc::from_virq(IRQ_IN_FLIGHT_TEST_VIRQ),
                    Some(IrqAction::regular(Arc::new(test_in_flight_handler))),
                ),
            );
        }

        let pending = PendingIrq::new(IrqRef::Virq(IRQ_IN_FLIGHT_TEST_VIRQ), 0);
        dispatch_actions(&pending);

        assert_eq!(IRQ_IN_FLIGHT_OBSERVED.load(Ordering::Relaxed), 1);
        {
            let state = IRQ_STATE.lock();
            let entry = state
                .descs
                .get(&IRQ_IN_FLIGHT_TEST_VIRQ)
                .expect("descriptor must remain while action exists");
            assert_eq!(entry.in_flight_for_tests(), 0);
        }

        assert!(unregister(IRQ_IN_FLIGHT_TEST_VIRQ).is_some());
    }

    #[def_test(serial)]
    fn test_in_flight_completion_reinitializes_between_dispatches() {
        let virq = 0x3140;
        let mut entry = IrqStateDesc::test_with_runtime(
            IrqDesc::from_virq(virq),
            Some(IrqAction::regular(Arc::new(test_handler))),
        );

        assert!(entry.in_flight_zero_completion().is_completed());

        entry.begin_dispatch();
        assert!(!entry.in_flight_zero_completion().is_completed());
        let wait_set = entry
            .finish_dispatch()
            .expect("last in-flight dispatch should produce completion wake");
        assert!(entry.in_flight_zero_completion().is_completed());
        wait_set.wake();

        entry.begin_dispatch();
        assert!(!entry.in_flight_zero_completion().is_completed());
        let wait_set = entry
            .finish_dispatch()
            .expect("second last in-flight dispatch should produce completion wake");
        assert!(entry.in_flight_zero_completion().is_completed());
        wait_set.wake();
    }

    #[def_test(serial)]
    fn test_in_flight_completion_old_wake_does_not_complete_next_dispatch() {
        let virq = 0x3143;
        let mut entry = IrqStateDesc::test_with_runtime(
            IrqDesc::from_virq(virq),
            Some(IrqAction::regular(Arc::new(test_handler))),
        );

        entry.begin_dispatch();
        let old_wait_set = entry
            .finish_dispatch()
            .expect("last in-flight dispatch should produce completion wake");
        assert!(entry.in_flight_zero_completion().is_completed());

        entry.begin_dispatch();
        assert!(!entry.in_flight_zero_completion().is_completed());
        old_wait_set.wake();
        assert!(
            !entry.in_flight_zero_completion().is_completed(),
            "a delayed wake from the previous in-flight generation must not mark the next one done"
        );

        let next_wait_set = entry
            .finish_dispatch()
            .expect("next dispatch should produce its own completion wake");
        assert!(entry.in_flight_zero_completion().is_completed());
        next_wait_set.wake();
    }

    #[def_test(serial)]
    fn test_in_flight_completion_wakes_multiple_waiters() {
        let virq = 0x3141;
        let first_counter = Box::leak(Box::new(AtomicUsize::new(0)));
        let second_counter = Box::leak(Box::new(AtomicUsize::new(0)));
        let first_waker = make_waker(first_counter);
        let second_waker = make_waker(second_counter);
        let first_cx = Context::from_waker(&first_waker);
        let second_cx = Context::from_waker(&second_waker);
        let mut first_registrations = PollRegistrations::new();
        let mut second_registrations = PollRegistrations::new();
        let mut entry = IrqStateDesc::test_with_runtime(
            IrqDesc::from_virq(virq),
            Some(IrqAction::regular(Arc::new(test_handler))),
        );

        entry.begin_dispatch();
        let wait_source = entry.in_flight_zero_completion();
        wait_source
            .register(&mut first_registrations.context(&first_cx))
            .unwrap();
        wait_source
            .register(&mut second_registrations.context(&second_cx))
            .unwrap();

        let wait_set = entry
            .finish_dispatch()
            .expect("last in-flight dispatch should produce completion wake");
        assert_eq!(first_counter.load(Ordering::SeqCst), 0);
        assert_eq!(second_counter.load(Ordering::SeqCst), 0);

        wait_set.wake();

        assert_eq!(first_counter.load(Ordering::SeqCst), 1);
        assert_eq!(second_counter.load(Ordering::SeqCst), 1);
    }

    #[def_test(serial)]
    fn test_free_irq_removes_action_and_unused_descriptor() {
        let virq = 0x3115;

        assert!(try_register(virq, Arc::new(test_handler)).expect("register should work"));
        assert!(
            try_free_irq(virq)
                .expect("free_irq should not fail")
                .is_some()
        );
        assert!(!IRQ_STATE.lock().descs.contains_key(&virq));
    }

    #[def_test(serial)]
    fn test_free_irq_waits_for_in_flight_dispatch() {
        let virq = 0x312e;
        IRQ_WAIT_TEST_TARGET.store(virq, Ordering::Release);
        IRQ_WAIT_TEST_OBSERVED.store(0, Ordering::Release);
        IRQ_WAIT_TEST_RELEASES.store(0, Ordering::Release);
        let old_hook = WAIT_FOR_IN_FLIGHT_HOOK
            .lock()
            .replace(test_release_in_flight_on_wait);

        assert!(try_register(virq, Arc::new(test_handler)).expect("register should work"));
        {
            let mut state = IRQ_STATE.lock();
            let entry = state.descs.get_mut(&virq).expect("descriptor must exist");
            entry.begin_dispatch();
        }

        assert!(
            try_free_irq(virq)
                .expect("free_irq should not fail")
                .is_some()
        );

        *WAIT_FOR_IN_FLIGHT_HOOK.lock() = old_hook;
        IRQ_WAIT_TEST_TARGET.store(0, Ordering::Release);
        assert_eq!(IRQ_WAIT_TEST_OBSERVED.load(Ordering::Acquire), 1);
        assert_eq!(IRQ_WAIT_TEST_RELEASES.load(Ordering::Acquire), 1);
        assert!(!IRQ_STATE.lock().descs.contains_key(&virq));
    }

    #[def_test(serial)]
    fn test_disable_irq_waits_after_nosync_disable() {
        let virq = 0x3116;

        assert!(try_register(virq, Arc::new(test_handler)).expect("register should work"));
        assert!(try_disable_irq(virq).expect("disable_irq should not fail"));
        {
            let state = IRQ_STATE.lock();
            let entry = state.descs.get(&virq).expect("descriptor must exist");
            assert_eq!(entry.disable_depth_for_tests(), 1);
            assert!(!entry.is_enabled_for_tests());
        }

        assert!(unregister(virq).is_some());
    }

    #[def_test]
    fn test_synchronize_irq_rejects_hardirq_context() {
        let _hardirq = crate::context::HardIrqContextGuard::enter();

        let err = try_synchronize_irq(0x3117).expect_err("context misuse must be an error");
        assert!(matches!(
            err,
            IrqDescError::InvalidContext {
                operation: "synchronize_irq"
            }
        ));
    }

    #[def_test(serial)]
    fn test_free_irq_action_rejects_hardirq_context() {
        let _hardirq = crate::context::HardIrqContextGuard::enter();

        let err = match try_free_irq_action(0x3118, crate::IrqActionToken::new(1)) {
            Err(err) => err,
            Ok(_) => panic!("context misuse must be an error"),
        };
        assert!(matches!(
            err,
            IrqDescError::InvalidContext {
                operation: "free_irq"
            }
        ));
    }

    #[def_test(serial)]
    fn test_synchronize_irq_waits_for_in_flight_dispatch() {
        let virq = 0x312f;
        IRQ_WAIT_TEST_TARGET.store(virq, Ordering::Release);
        IRQ_WAIT_TEST_OBSERVED.store(0, Ordering::Release);
        IRQ_WAIT_TEST_RELEASES.store(0, Ordering::Release);
        let old_hook = WAIT_FOR_IN_FLIGHT_HOOK
            .lock()
            .replace(test_release_in_flight_on_wait);

        assert!(try_register(virq, Arc::new(test_handler)).expect("register should work"));
        {
            let mut state = IRQ_STATE.lock();
            let entry = state.descs.get_mut(&virq).expect("descriptor must exist");
            entry.begin_dispatch();
        }

        assert!(try_synchronize_irq(virq).expect("synchronize_irq should not fail"));

        *WAIT_FOR_IN_FLIGHT_HOOK.lock() = old_hook;
        IRQ_WAIT_TEST_TARGET.store(0, Ordering::Release);
        assert_eq!(IRQ_WAIT_TEST_OBSERVED.load(Ordering::Acquire), 1);
        assert_eq!(IRQ_WAIT_TEST_RELEASES.load(Ordering::Acquire), 1);
        {
            let state = IRQ_STATE.lock();
            let entry = state.descs.get(&virq).expect("descriptor must remain");
            assert_eq!(entry.in_flight_for_tests(), 0);
        }
        assert!(unregister(virq).is_some());
    }

    #[def_test]
    fn test_legacy_enable_forces_platform_enable_when_already_enabled() {
        let virq = 0x3114;
        let mut entry = IrqStateDesc::new(IrqDesc::from_virq(virq));

        let first = entry.prepare_auto_enable();
        assert_eq!(first.enable, Some(true));
        assert!(entry.is_enabled_for_tests());

        let depth_aware = entry.prepare_enable_irq();
        assert_eq!(depth_aware.enable, None);

        let legacy = entry.prepare_legacy_enable();
        assert_eq!(legacy.enable, Some(true));
    }

    #[def_test]
    fn test_reconfigure_if_stale_does_not_toggle_enabled_state() {
        let virq = 0x3142;
        let mut entry = IrqStateDesc::new(IrqDesc::from_virq(virq));

        let first = entry.prepare_auto_enable();
        assert!(first.configure);
        assert_eq!(first.enable, Some(true));
        assert_eq!(entry.configured_generation_for_tests(), Some(0));
        assert!(entry.is_enabled_for_tests());

        let unchanged = entry.prepare_reconfigure_if_stale();
        assert!(!unchanged.configure);
        assert_eq!(unchanged.enable, None);
        assert!(entry.is_enabled_for_tests());

        entry.update_desc(IrqDesc::from_virq(virq).with_controller(IrqController::Gic));
        let stale = entry.prepare_reconfigure_if_stale();
        assert!(stale.configure);
        assert_eq!(stale.enable, None);
        assert_eq!(entry.configured_generation_for_tests(), Some(1));
        assert!(entry.is_enabled_for_tests());
    }

    #[def_test(serial)]
    fn test_single_regular_action_dispatch_remains_compatible() {
        let virq = 0x3102;
        REGULAR_CALLS.store(0, Ordering::Relaxed);

        {
            let mut state = IRQ_STATE.lock();
            state.descs.insert(
                virq,
                IrqStateDesc::test_with_runtime(
                    IrqDesc::from_virq(virq),
                    Some(IrqAction::regular(Arc::new(test_not_handled_handler))),
                ),
            );
        }

        let pending = PendingIrq::new(IrqRef::Virq(virq), virq);
        dispatch_actions(&pending);

        assert_eq!(REGULAR_CALLS.load(Ordering::Relaxed), 1);
        assert!(unregister(virq).is_some());
    }

    #[def_test(serial)]
    fn test_irq_handler_receives_resolved_virq() {
        let virq = 0x3104;
        HANDLER_OBSERVED_IRQ.store(0, Ordering::Relaxed);

        assert!(try_register(virq, Arc::new(test_observe_irq_handler)).unwrap());
        let pending = PendingIrq::new(IrqRef::Virq(virq), virq);
        dispatch_actions(&pending);

        assert_eq!(HANDLER_OBSERVED_IRQ.load(Ordering::Relaxed), virq);
        assert!(unregister(virq).is_some());
    }

    #[def_test(serial)]
    fn test_unsupported_threaded_action_does_not_run_primary() {
        let virq = 0x3103;
        REGULAR_CALLS.store(0, Ordering::Relaxed);

        {
            let mut state = IRQ_STATE.lock();
            state.descs.insert(
                virq,
                IrqStateDesc::test_with_runtime(
                    IrqDesc::from_virq(virq),
                    Some(IrqAction::test_new(
                        Arc::new(test_handler),
                        Some(IrqThreadSlot),
                        IrqActionFlags::NO_THREAD,
                        Some("future-threaded"),
                    )),
                ),
            );
        }

        let pending = PendingIrq::new(IrqRef::Virq(virq), virq);
        dispatch_actions(&pending);

        assert_eq!(REGULAR_CALLS.load(Ordering::Relaxed), 0);
        assert!(
            IRQ_STATE
                .lock()
                .descs
                .get(&virq)
                .expect("descriptor must remain")
                .has_actions()
        );
        assert!(unregister(virq).is_some());
    }

    #[def_test(serial)]
    fn test_unregister_keeps_msi_descriptor_for_free_msix() {
        let (hwirq, virq) = {
            let mut state = IRQ_STATE.lock();
            let (hwirq, virq) =
                unused_msi_test_mapping(&state).expect("MSI test mapping space exhausted");
            let desc = IrqDesc::new(hwirq, IrqTrigger::EdgeRising)
                .with_domain(MSI_DOMAIN)
                .with_flags(IrqFlags::MSI)
                .with_virq(virq);
            let desc = state
                .try_resolve_desc(desc)
                .expect("MSI descriptor should be accepted")
                .into_desc();
            let entry = state
                .descs
                .get_mut(&desc.logical_irq().unwrap())
                .expect("descriptor state must exist after try_resolve_desc");
            assert!(entry.install_regular_action(IrqAction::regular(Arc::new(test_handler))));
            (hwirq, virq)
        };

        assert!(unregister(virq).is_some());
        {
            let state = IRQ_STATE.lock();
            let entry = state.descs.get(&virq).expect("MSI descriptor must stay");
            assert!(entry.is_unused());
            assert_eq!(state.translated_hwirq(MSI_DOMAIN, hwirq), Some(virq));
        }
        {
            let mut state = IRQ_STATE.lock();
            assert_eq!(state.remove_msi_if_unused(virq), Some(hwirq));
            assert_eq!(state.translated_hwirq(MSI_DOMAIN, hwirq), None);
            assert!(!state.descs.contains_key(&virq));
        }

        let new_hwirq = {
            let state = IRQ_STATE.lock();
            unused_msi_test_hwirq_except(&state, hwirq).expect("MSI test hwirq space exhausted")
        };
        let remapped_virq = try_map(
            IrqDesc::new(new_hwirq, IrqTrigger::EdgeRising)
                .with_domain(MSI_DOMAIN)
                .with_flags(IrqFlags::MSI)
                .with_virq(virq),
        )
        .expect("freed virq mapping slot should be reusable");
        assert_eq!(remapped_virq, virq);
        {
            let mut state = IRQ_STATE.lock();
            assert_eq!(state.remove_msi_if_unused(virq), Some(new_hwirq));
            assert_eq!(state.translated_hwirq(MSI_DOMAIN, new_hwirq), None);
        }
    }
}

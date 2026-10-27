// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IRQ manager public API and dispatch-entry orchestration.
//!
//! Management is not NMI-safe. Checked map/enable/disable/register/free/sync
//! entry points reject NMI before locking. Compatibility wrappers also suppress
//! their usual failure log in NMI, since the logger may use ordinary locks.

use alloc::sync::Arc;

use kiface_task_api::TaskBlockingContextIf;
use krcu_core::RcuContextIf;
use kspin::{SpinNoIrq, static_lock};

use super::rcu::with_nmi_context;
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
    state::{IRQ_STATE, IrqLine, IrqPlatformPlan, IrqState, IrqStateDesc, try_resolve_and_publish},
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
    nmi::{register_nmi, register_nmi_startup, unregister_nmi},
    platform::{
        DispatchedIrq, Handler, IntrManagerIf, PendingIrq, TargetCpu, notify_cpu, set_prio,
    },
    state::DYNAMIC_VIRQ_BASE,
};

static_lock! {
    pub(super) static IRQ_CONTROL_LOCK: SpinNoIrq<()> = SpinNoIrq::new(());
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
    reject_nmi_management("map IRQ")?;
    let spec = IrqSpec::Desc(desc);
    let mut prepared = None;
    loop {
        let mut state = IRQ_STATE.lock();
        if lookup_line(&state, spec).is_none() && prepared.is_none() {
            drop(state);
            prepared = Some(prepare_line(spec));
            continue;
        }
        #[expect(
            clippy::unwrap_used,
            reason = "CG-021-no-panic-on-reachable-failure: try_resolve_and_publish returns only \
                      descriptors assigned a logical IRQ"
        )]
        return Ok(
            try_resolve_and_publish(&mut state, spec, prepared.as_ref())?
                .logical_irq()
                .unwrap(),
        );
    }
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
        report_management_error("enable IRQ", &err);
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
        report_management_error("enable IRQ", &err);
    }
}

/// Try to enable an IRQ line after a previous disable or disabled registration.
pub fn try_enable_irq(spec: impl Into<IrqSpec>) -> Result<(), IrqDescError> {
    try_enable_irq_inner(spec.into(), false)
}

fn try_enable_irq_inner(spec: IrqSpec, force_platform_enable: bool) -> Result<(), IrqDescError> {
    reject_nmi_management("enable IRQ")?;
    let mut prepared = None;
    loop {
        if force_platform_enable && prepared.is_none() {
            let is_missing = {
                let state = IRQ_STATE.lock();
                lookup_line(&state, spec).is_none()
            };
            if is_missing {
                prepared = Some(prepare_line(spec));
            }
        }
        let control_guard = IRQ_CONTROL_LOCK.lock();
        let plan = {
            let mut state = IRQ_STATE.lock();
            if force_platform_enable {
                if lookup_line(&state, spec).is_none() && prepared.is_none() {
                    drop(state);
                    drop(control_guard);
                    continue;
                }
                if let Some((virq, line)) = lookup_line(&state, spec) {
                    reject_teardown(Some(&line), virq, "enable IRQ")?;
                }
                let desc = try_resolve_and_publish(&mut state, spec, prepared.as_ref())?;
                #[expect(
                    clippy::unwrap_used,
                    reason = "CG-021-no-panic-on-reachable-failure: try_resolve_and_publish \
                              succeeded and assigned this descriptor a logical IRQ"
                )]
                let virq = desc.logical_irq().unwrap();
                #[expect(
                    clippy::expect_used,
                    reason = "CG-021-no-panic-on-reachable-failure: try_resolve_and_publish \
                              installed this descriptor in the same exclusively borrowed registry"
                )]
                let mut entry = state
                    .descs
                    .get(&virq)
                    .expect("descriptor state must exist after try_resolve_desc")
                    .lock();
                entry.prepare_legacy_enable()
            } else {
                let virq = state.lookup_virq(spec).ok_or(IrqDescError::UnknownIrq)?;
                let line = state.descs.get(&virq).ok_or(IrqDescError::UnknownIrq)?;
                reject_teardown(Some(line), virq, "enable IRQ")?;
                let mut entry = line.lock();
                if !entry.has_actions() {
                    warn!("enable IRQ {virq} failed because no IRQ action is registered");
                    return Err(IrqDescError::NoIrqAction { virq });
                }
                entry.prepare_enable_irq()
            }
        };
        apply_platform_plan(plan);
        return Ok(());
    }
}

/// Disable an IRQ line without waiting for an in-flight handler to finish.
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
pub fn try_disable_irq_nosync(spec: impl Into<IrqSpec>) -> Result<(), IrqDescError> {
    reject_nmi_management("disable IRQ")?;
    let spec = spec.into();
    let _control_guard = IRQ_CONTROL_LOCK.lock();
    let plan = {
        let state = IRQ_STATE.lock();
        let virq = state.lookup_virq(spec).ok_or(IrqDescError::UnknownIrq)?;
        let mut entry = state
            .descs
            .get(&virq)
            .ok_or(IrqDescError::UnknownIrq)?
            .lock();
        entry.prepare_disable_irq_nosync()
    };
    apply_platform_plan(plan);
    Ok(())
}

/// Disable an IRQ line and wait for currently running handlers to finish.
///
/// Requires sleepable task context accepted by [`TaskBlockingContextIf::is_sleepable_context`],
/// even when no handler is currently in flight. Returns `false` for an unknown
/// descriptor or invalid context; use [`try_disable_irq`] to distinguish them.
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
    let drain = {
        let _control_guard = IRQ_CONTROL_LOCK.lock();
        let state = IRQ_STATE.lock();
        let virq = state.lookup_virq(spec).ok_or(IrqDescError::UnknownIrq)?;
        let line = state.descs.get(&virq).ok_or(IrqDescError::UnknownIrq)?;
        let drain = IrqDrain::new(virq, line);
        let mut entry = line.lock();
        let plan = entry.prepare_disable_irq_nosync();
        drop(entry);
        drop(state);
        apply_platform_plan(plan);
        drain
    };
    drain.wait();
    Ok(true)
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
            report_management_error("synchronize IRQ", &err);
            false
        }
    }
}

/// Try to wait until the current software hardirq dispatch for an IRQ line has finished.
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
        state
            .descs
            .get(&virq)
            .map(|entry| IrqDrain::new(virq, entry))
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
            let (in_flight, completion) = {
                let entry = self.line.lock();
                let in_flight = entry.in_flight();
                if in_flight == 0 {
                    return;
                }
                (in_flight, entry.in_flight_zero_completion())
            };
            observe_wait_for_in_flight_for_tests(self.virq, in_flight);
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
/// Registration requires sleepable task context, including when uncontended;
/// boot callers use [`try_register_startup`] after installing the current task.
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
pub fn try_register_with_options(
    spec: impl Into<IrqSpec>,
    handler: Handler,
    options: IrqRegisterOptions,
) -> Result<bool, IrqDescError> {
    register_with_mode(spec.into(), handler, options, ManagementMode::Task)
}

/// Registers during explicit startup after current-task installation.
///
/// Never waits for management exclusion. Returns false on contention or an
/// occupied Line. IRQs may be masked, but interrupt/BH context is rejected.
/// Call before source activation; this is not an atomic-context fallback.
pub fn try_register_startup(
    spec: impl Into<IrqSpec>,
    handler: Handler,
    options: IrqRegisterOptions,
) -> Result<bool, IrqDescError> {
    register_with_mode(spec.into(), handler, options, ManagementMode::Startup)
}

fn register_with_mode(
    spec: IrqSpec,
    handler: Handler,
    options: IrqRegisterOptions,
    mode: ManagementMode,
) -> Result<bool, IrqDescError> {
    // Keep a caller-owned reference until the scoped mutex has been released,
    // including rejection and installation failure.
    let result = with_registration_line(spec, mode, |virq, line| {
        let state = IRQ_STATE.lock();
        let mut entry = line.lock();
        if entry.has_actions() {
            warn!("register handler for IRQ {virq} failed");
            return Ok(false);
        }
        let installed = entry.install_regular_action(IrqAction::regular(handler.clone()));
        debug_assert!(installed, "duplicate regular action was checked above");
        let plan = if options.should_auto_enable() {
            entry.prepare_auto_enable()
        } else {
            entry.prepare_register_disabled()
        };
        drop(entry);
        drop(state);
        apply_platform_plan(plan);
        Ok(true)
    });
    drop(handler);
    result.map(|registered| registered.unwrap_or(false))
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
            report_management_error("register shared IRQ handler", &err);
            None
        }
    }
}

/// Try to register one shared IRQ action with default auto-enable behavior.
///
/// Requires sleepable task context; waits for this Line's management mutex.
pub fn try_register_shared(
    spec: impl Into<IrqSpec>,
    handler: Handler,
) -> Result<Option<IrqActionToken>, IrqDescError> {
    let result = with_registration_line(spec.into(), ManagementMode::Task, |virq, line| {
        let state = IRQ_STATE.lock();
        let mut entry = line.lock();
        let was_first_action = !entry.has_actions();
        let Some(token) = entry.install_shared_action(handler.clone()) else {
            warn!("register shared handler for IRQ {virq} failed");
            return Ok(None);
        };
        let plan = if was_first_action {
            entry.prepare_auto_enable()
        } else {
            entry.prepare_reconfigure_if_stale()
        };
        drop(entry);
        drop(state);
        apply_platform_plan(plan);
        Ok(Some(token))
    });
    drop(handler);
    result.map(Option::flatten)
}

/// Acquisition policy is explicit; atomic runtime callers never fall back to startup.
#[derive(Clone, Copy)]
pub(super) enum ManagementMode {
    Task,
    Startup,
}

fn validate_management(mode: ManagementMode) -> Result<(), IrqDescError> {
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

/// Membership pin; the caller keeps its mutex guard alive until this drops.
struct ManagementAdmission<'a> {
    line: &'a Arc<IrqLine>,
    virq: Option<Virq>,
    is_active: bool,
    has_removed_action: bool,
}

impl Drop for ManagementAdmission<'_> {
    fn drop(&mut self) {
        if !self.is_active {
            return;
        }
        let _control_guard = IRQ_CONTROL_LOCK.lock();
        let mut state = IRQ_STATE.lock();
        {
            let mut entry = self.line.lock();
            if self.has_removed_action {
                entry.finish_teardown();
            }
            entry.end_management();
        }
        if self.has_removed_action
            && let Some(virq) = self.virq
            && state
                .descs
                .get(&virq)
                .is_some_and(|line| Arc::ptr_eq(line, self.line))
        {
            state.remove_if_unused_with_desc(virq);
        }
    }
}

/// Runs registration with an admitted Line, retrying stale directory identities.
///
/// The body runs under management and control exclusion, without registry or
/// Line guards. It must not wait or reacquire the control lock. Return discarded
/// handler owners rather than finally destroying them inside the transaction.
/// None means startup contention or teardown rejection.
pub(super) fn with_registration_line<T>(
    spec: IrqSpec,
    mode: ManagementMode,
    body_fn: impl FnOnce(Virq, &Arc<IrqLine>) -> Result<T, IrqDescError>,
) -> Result<Option<T>, IrqDescError> {
    with_registration_line_checked(spec, mode, |_| Ok(true), body_fn)
}

/// Checks registration-specific rejection before merging or publishing metadata.
///
/// The preflight runs under management, control and registry exclusion, but
/// without a Line guard. It may lock the supplied Line or the NMI table, but
/// must not reacquire the registry/control locks, allocate or wait. False
/// rejects without resolution or publication. The ordinary body retains control
/// exclusion across publication, but runs without registry or Line guards. It
/// must not wait or reacquire the control lock. This makes a shared-table check
/// and publication atomic even when registrations select different Lines.
#[expect(
    clippy::expect_used,
    reason = "CG-021-no-panic-on-reachable-failure: successful resolution assigns a virq"
)]
pub(super) fn with_registration_line_checked<T>(
    spec: IrqSpec,
    mode: ManagementMode,
    mut preflight_fn: impl FnMut(&Arc<IrqLine>) -> Result<bool, IrqDescError>,
    body_fn: impl FnOnce(Virq, &Arc<IrqLine>) -> Result<T, IrqDescError>,
) -> Result<Option<T>, IrqDescError> {
    validate_management(mode)?;
    // Identity retries happen before publication or mutation. Retain only our
    // unpublished candidate; never reuse a retired, previously published Line.
    let mut candidate = None;
    loop {
        let existing = {
            let state = IRQ_STATE.lock();
            lookup_line(&state, spec)
        };
        let is_candidate = existing.is_none();
        let line = match existing {
            Some((_, line)) => line,
            None => Arc::clone(candidate.get_or_insert_with(|| prepare_line(spec))),
        };
        let Some(_management_guard) = acquire_management(&line, mode)? else {
            return Ok(None);
        };
        let mut admission = ManagementAdmission {
            line: &line,
            virq: None,
            is_active: false,
            has_removed_action: false,
        };
        // Drop control before admission cleanup reacquires it, then release
        // the management mutex only after that cleanup finishes.
        let _control_guard = IRQ_CONTROL_LOCK.lock();
        let virq = {
            let mut state = IRQ_STATE.lock();
            match lookup_line(&state, spec) {
                Some((_, current)) if !is_candidate && Arc::ptr_eq(&current, &line) => {}
                None if is_candidate => {}
                _ => continue,
            }
            if line.lock().is_teardown_in_progress() {
                return Ok(None);
            }
            if !preflight_fn(&line)? {
                return Ok(None);
            }
            line.lock().begin_management();
            admission.is_active = true;
            let desc = try_resolve_and_publish(&mut state, spec, Some(&line))?;
            let virq = desc.logical_irq().expect("resolved descriptor has a virq");
            admission.virq = Some(virq);
            virq
        };
        return body_fn(virq, &line).map(Some);
    }
}

/// Runs removal on the originally selected Line, never a same-number successor.
///
/// Missing/stale entries and startup contention return None without executing
/// the body. Admission pins membership across drain. The body returns Some only
/// after successful removal; only then may unused cleanup follow under control
/// exclusion, while management exclusion is still held. None or an error leaves
/// idle mappings intact. Return removed owners out of the body so their final
/// destruction occurs after mutex release.
/// A successful body must begin teardown before withdrawal and leave it set:
/// completion, admission release and unused removal form one control transaction.
pub(super) fn with_existing_line<T>(
    spec: IrqSpec,
    mode: ManagementMode,
    body_fn: impl FnOnce(Virq, &Arc<IrqLine>) -> Result<Option<T>, IrqDescError>,
) -> Result<Option<T>, IrqDescError> {
    validate_management(mode)?;
    let selected = {
        let state = IRQ_STATE.lock();
        lookup_line(&state, spec)
    };
    let Some((virq, line)) = selected else {
        return Ok(None);
    };
    let Some(_management_guard) = acquire_management(&line, mode)? else {
        return Ok(None);
    };
    let mut admission = ManagementAdmission {
        line: &line,
        virq: Some(virq),
        is_active: false,
        has_removed_action: false,
    };
    {
        let state = IRQ_STATE.lock();
        if !state
            .descs
            .get(&virq)
            .is_some_and(|current| Arc::ptr_eq(current, &line))
        {
            return Ok(None);
        }
        line.lock().begin_management();
        admission.is_active = true;
    }
    let result = body_fn(virq, &line)?;
    admission.has_removed_action = result.is_some();
    Ok(result)
}

fn reject_teardown(
    entry: Option<&Arc<IrqLine>>,
    virq: Virq,
    operation: &'static str,
) -> Result<(), IrqDescError> {
    if entry.is_some_and(|line| line.lock().is_teardown_in_progress()) {
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
/// An NMI-owned fallback is not a regular registration and cannot be removed
/// here. Its owner must use [`unregister_nmi`] after quiescing the source.
///
/// The line is masked once the last regular action is removed. The API
/// returns after any handler snapshot that was already running has exited, so
/// device teardown can safely release MMIO or DMA state owned by the handler.
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
/// Requires sleepable task context accepted by [`TaskBlockingContextIf::is_sleepable_context`],
/// even when no handler is in flight. Context misuse is reported as
/// [`IrqDescError::InvalidContext`] without removing the action.
///
/// Callers must not hold locks or resources that the IRQ handler may acquire
/// while waiting, otherwise the handler can block behind its own teardown.
///
/// Context is validated before action removal; subsequent waiting is infallible.
pub fn try_free_irq(spec: impl Into<IrqSpec>) -> Result<Option<Handler>, IrqDescError> {
    try_free_irq_inner(spec.into(), None)
}

/// Remove one shared IRQ action by token and synchronize dispatch teardown.
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
/// Requires sleepable task context accepted by [`TaskBlockingContextIf::is_sleepable_context`],
/// even when no handler is in flight. Context misuse is reported as
/// [`IrqDescError::InvalidContext`] without removing the action.
///
/// Callers must not hold locks or resources that the IRQ handler may acquire
/// while waiting, otherwise the handler can block behind its own teardown.
///
/// Context is validated before action removal; subsequent waiting is infallible.
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
    with_existing_line(spec, ManagementMode::Task, |virq, line| {
        let (drain, removed_action) = {
            let _control_guard = IRQ_CONTROL_LOCK.lock();
            let state = IRQ_STATE.lock();
            let mut entry = line.lock();
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
                        "free_irq for IRQ {virq} rejected: action belongs to shared or NMI \
                         registration; use its owning removal API"
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

            let drain = IrqDrain::new(virq, line);
            drop(entry);
            drop(state);
            if let Some(plan) = disable_plan {
                apply_platform_plan(plan);
            }
            (drain, removed_action)
        };
        if removed_action.is_none() {
            return Ok(None);
        }
        drain.wait();
        Ok(removed_action.map(IrqAction::into_primary))
    })
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
unittest::path_mod_test!(
    tests_management_identity,
    "../../ut-tests/management_identity.rs"
);

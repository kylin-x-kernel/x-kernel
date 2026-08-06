// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IRQ manager public API and dispatch-entry orchestration.

use crate::{
    Hwirq, IrqDesc, IrqDescError, IrqDomainId, IrqSpec, Virq,
    action::IrqAction,
    context::HardIrqContextGuard,
    dispatch::dispatch_subscribers,
    lifecycle::IrqLifecycleGuard,
    nmi::dispatch_nmi_handler,
    platform::{
        configure_and_enable_platform_irq, disable_platform_irq, platform_dispatch_irq,
        platform_dispatch_nmi,
    },
    state::{
        IRQ_STATE, IrqStateDesc, WakeHandler, WakeSubscription, WakeupMode, try_resolve_and_publish,
    },
};
pub use crate::{
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

/// Try to configure and enable or disable an IRQ line.
#[inline]
pub fn try_enable(spec: impl Into<IrqSpec>, on: bool) -> Result<(), IrqDescError> {
    let desc = {
        let mut state = IRQ_STATE.lock();
        try_resolve_and_publish(&mut state, spec.into())?
    };
    configure_and_enable_platform_irq(desc, on);
    Ok(())
}

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
    let mut state = IRQ_STATE.lock();
    let desc = try_resolve_and_publish(&mut state, spec.into())?;
    let virq = desc.logical_irq().unwrap();
    let entry = state
        .descs
        .get_mut(&virq)
        .expect("descriptor state must exist after try_resolve_desc");
    if entry.has_regular_action() {
        warn!("register handler for IRQ {virq} failed");
        return Ok(false);
    }
    let installed = entry.install_regular_action(IrqAction::regular(handler));
    debug_assert!(installed, "duplicate regular action was checked above");
    let desc = entry.desc;
    drop(state);
    configure_and_enable_platform_irq(desc, true);
    Ok(true)
}

/// Remove the regular OS IRQ handler for an IRQ line.
pub fn unregister(spec: impl Into<IrqSpec>) -> Option<Handler> {
    let spec = spec.into();
    let mut state = IRQ_STATE.lock();
    let virq = state.lookup_virq(spec)?;
    let (regular_action, removed_wakeup) = state
        .descs
        .get_mut(&virq)
        .map(|entry| {
            (
                entry.take_regular_action(),
                entry.take_wake_subscription().is_some(),
            )
        })
        .unwrap_or((None, false));
    if regular_action.is_some() {
        let disabled_desc = state.remove_if_unused_with_desc(virq);
        drop(state);
        if let Some(desc) = disabled_desc {
            disable_platform_irq(desc);
        }
        return regular_action.map(IrqAction::into_primary);
    }
    if removed_wakeup {
        warn!("removed stale wakeup subscription for IRQ {virq} without regular handler");
        let disabled_desc = state.remove_if_unused_with_desc(virq);
        drop(state);
        if let Some(desc) = disabled_desc {
            disable_platform_irq(desc);
        }
    }
    regular_action.map(IrqAction::into_primary)
}

// Wakeup subscriptions do not install a regular IRQ handler. They keep the IRQ
// bound so the IRQ core can observe the event and run the wakeup callback path.
fn subscribe_wakeup_mode(spec: impl Into<IrqSpec>, mode: WakeupMode, handler: WakeHandler) -> bool {
    let mut state = IRQ_STATE.lock();
    let desc = match try_resolve_and_publish(&mut state, spec.into()) {
        Ok(desc) => desc,
        Err(err) => {
            warn!("subscribe wakeup failed: {err:?}");
            return false;
        }
    };
    let virq = desc.logical_irq().unwrap();
    let entry = state
        .descs
        .get_mut(&virq)
        .expect("descriptor state must exist after try_resolve_desc");
    if !entry.has_regular_action() {
        warn!("subscribe wakeup for IRQ {virq} without regular handler");
        return false;
    }
    let installed = entry.install_wake_subscription(WakeSubscription {
        mode,
        armed: true,
        handler,
    });
    debug_assert!(installed, "regular action was checked above");
    let desc = entry.desc;
    drop(state);
    configure_and_enable_platform_irq(desc, true);
    true
}

pub fn subscribe_wakeup(spec: impl Into<IrqSpec>, handler: WakeHandler) -> bool {
    subscribe_wakeup_mode(spec, WakeupMode::Persistent, handler)
}

pub fn subscribe_wakeup_once(spec: impl Into<IrqSpec>, handler: WakeHandler) -> bool {
    subscribe_wakeup_mode(spec, WakeupMode::OneShot, handler)
}

pub fn unsubscribe_wakeup(spec: impl Into<IrqSpec>) -> bool {
    let spec = spec.into();
    let mut state = IRQ_STATE.lock();
    let Some(virq) = state.lookup_virq(spec) else {
        return false;
    };
    let removed = state
        .descs
        .get_mut(&virq)
        .and_then(IrqStateDesc::take_wake_subscription)
        .is_some();
    if removed {
        let disabled_desc = state.remove_if_unused_with_desc(virq);
        drop(state);
        if let Some(desc) = disabled_desc {
            disable_platform_irq(desc);
        }
        return true;
    }
    false
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
/// 4. run regular action and wake compatibility fanout outside `IRQ_STATE`;
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
    let resolved_irq = dispatch_subscribers(&pending_irq);
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
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicUsize, Ordering};

    use unittest::def_test;

    use super::{
        DYNAMIC_VIRQ_BASE, IRQ_STATE, IrqDesc, IrqDescError, IrqDomainId, IrqStateDesc,
        WakeSubscription, WakeupMode, dispatch_subscribers, run_claimed_irq_tail, try_map,
        try_register, unregister,
    };
    use crate::{
        GIC_ROOT_DOMAIN, IrqEvent, IrqFlags, IrqLifecycleHooks, IrqRef, IrqTrigger, MSI_DOMAIN,
        PendingIrq,
        action::{IrqAction, IrqActionFlags, IrqThreadSlot},
        clear_irq_lifecycle_hooks,
        lifecycle::IrqLifecycleGuard,
        register_irq_lifecycle_hooks,
        softirq::{
            SoftirqRunResult, SoftirqVec, local_softirq_pending, open_softirq, raise_softirq,
            reset_softirq_for_tests, run_pending_softirqs, softirq_diagnostics,
        },
    };

    static IRQ_LIFECYCLE_TEST_LOCK: kspin::SpinNoIrq<()> = kspin::SpinNoIrq::new(());
    static REGULAR_CALLS: AtomicUsize = AtomicUsize::new(0);
    static WAKE_CALLS: AtomicUsize = AtomicUsize::new(0);
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

    const IRQ_TAIL_TEST_VECTOR: usize = 0xdead;
    const IRQ_TAIL_TEST_VIRQ: usize = 0x2f02;
    const IRQ_TAIL_SOFTIRQ_VIRQ: usize = 0x2f03;
    const IRQ_SIMULATED_TEST_VECTOR: usize = 0x5f00;
    const IRQ_SIMULATED_TEST_VIRQ: usize = 0x5f01;
    const IRQ_LIFECYCLE_TEST_VECTOR: usize = 0x5f10;
    const IRQ_LIFECYCLE_TEST_VIRQ: usize = 0x5f11;
    const MSI_TEST_HWIRQ_BASE: usize = 0x30_000;
    const MSI_TEST_VIRQ_BASE: usize = 0x40_000;

    fn test_handler() -> IrqEvent {
        REGULAR_CALLS.fetch_add(1, Ordering::Relaxed);
        IrqEvent::HANDLED
    }

    fn test_not_handled_handler() -> IrqEvent {
        REGULAR_CALLS.fetch_add(1, Ordering::Relaxed);
        IrqEvent::NOT_HANDLED
    }

    fn test_wake_handler(_irq: usize) {
        WAKE_CALLS.fetch_add(1, Ordering::Relaxed);
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

    fn test_irq_tail_handler() -> IrqEvent {
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

    fn test_irq_tail_raise_softirq_handler() -> IrqEvent {
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
            (state.translated_hwirq(MSI_DOMAIN, hwirq).is_none()
                && !state.descs.contains_key(&virq))
            .then_some((hwirq, virq))
        })
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
                    None,
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
        IRQ_STATE.lock().remove_if_unused(virq);
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
                    None,
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
                    None,
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
                    None,
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
        dispatch_subscribers(&pending);

        assert_eq!(REGULAR_CALLS.load(Ordering::Relaxed), 1);
        assert!(unregister(virq).is_some());
    }

    #[def_test(serial)]
    fn test_single_regular_action_dispatch_remains_compatible() {
        let virq = 0x3102;
        REGULAR_CALLS.store(0, Ordering::Relaxed);
        WAKE_CALLS.store(0, Ordering::Relaxed);

        {
            let mut state = IRQ_STATE.lock();
            state.descs.insert(
                virq,
                IrqStateDesc::test_with_runtime(
                    IrqDesc::from_virq(virq),
                    Some(IrqAction::regular(Arc::new(test_not_handled_handler))),
                    Some(WakeSubscription {
                        mode: WakeupMode::Persistent,
                        armed: true,
                        handler: test_wake_handler,
                    }),
                ),
            );
        }

        let pending = PendingIrq::new(IrqRef::Virq(virq), virq);
        dispatch_subscribers(&pending);

        assert_eq!(REGULAR_CALLS.load(Ordering::Relaxed), 1);
        assert_eq!(WAKE_CALLS.load(Ordering::Relaxed), 1);
        assert!(unregister(virq).is_some());
    }

    #[def_test(serial)]
    fn test_unsupported_threaded_action_does_not_consume_oneshot_wakeup() {
        let virq = 0x3103;
        REGULAR_CALLS.store(0, Ordering::Relaxed);
        WAKE_CALLS.store(0, Ordering::Relaxed);

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
                    Some(WakeSubscription {
                        mode: WakeupMode::OneShot,
                        armed: true,
                        handler: test_wake_handler,
                    }),
                ),
            );
        }

        let pending = PendingIrq::new(IrqRef::Virq(virq), virq);
        dispatch_subscribers(&pending);

        assert_eq!(REGULAR_CALLS.load(Ordering::Relaxed), 0);
        assert_eq!(WAKE_CALLS.load(Ordering::Relaxed), 0);
        assert!(
            IRQ_STATE
                .lock()
                .descs
                .get(&virq)
                .expect("descriptor must remain")
                .has_wake_subscription()
        );
        assert!(unregister(virq).is_some());
    }

    #[def_test(serial)]
    fn test_unregister_clears_wakeup_subscription() {
        let virq = 0x2001;
        {
            let mut state = IRQ_STATE.lock();
            state.descs.insert(
                virq,
                IrqStateDesc::test_with_runtime(
                    IrqDesc::from_virq(virq),
                    Some(IrqAction::regular(Arc::new(test_handler))),
                    Some(WakeSubscription {
                        mode: WakeupMode::Persistent,
                        armed: true,
                        handler: test_wake_handler,
                    }),
                ),
            );
        }

        assert!(unregister(virq).is_some());
        assert!(!IRQ_STATE.lock().descs.contains_key(&virq));
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
    }

    #[def_test(serial)]
    fn test_oneshot_wakeup_is_removed_after_dispatch() {
        let virq = 0x2002;
        REGULAR_CALLS.store(0, Ordering::Relaxed);
        WAKE_CALLS.store(0, Ordering::Relaxed);

        {
            let mut state = IRQ_STATE.lock();
            state.descs.insert(
                virq,
                IrqStateDesc::test_with_runtime(
                    IrqDesc::from_virq(virq),
                    Some(IrqAction::regular(Arc::new(test_handler))),
                    Some(WakeSubscription {
                        mode: WakeupMode::OneShot,
                        armed: true,
                        handler: test_wake_handler,
                    }),
                ),
            );
        }

        let pending = PendingIrq::new(IrqRef::Virq(virq), virq);
        dispatch_subscribers(&pending);

        assert_eq!(REGULAR_CALLS.load(Ordering::Relaxed), 1);
        assert_eq!(WAKE_CALLS.load(Ordering::Relaxed), 1);
        let state = IRQ_STATE.lock();
        let entry = state
            .descs
            .get(&virq)
            .expect("descriptor must stay for regular handler");
        assert!(entry.has_regular_action());
        assert!(!entry.has_wake_subscription());
    }
}

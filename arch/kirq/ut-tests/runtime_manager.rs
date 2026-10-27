// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kcompletion::SyncWake;
use unittest::def_test;

use super::{
    DYNAMIC_VIRQ_BASE, IRQ_STATE, IrqDesc, IrqDescError, IrqDomainId, IrqStateDesc,
    WAIT_FOR_IN_FLIGHT_HOOK, dispatch_actions, run_claimed_irq_tail,
    run_irq_exit_tail_with_context, try_disable_irq, try_disable_irq_nosync, try_enable,
    try_enable_irq, try_free_irq, try_free_irq_action, try_map, try_register,
    try_register_disabled, try_register_shared, try_synchronize_irq, unregister,
};
use crate::{
    GIC_ROOT_DOMAIN, IrqController, IrqEvent, IrqFlags, IrqLifecycleHooks, IrqRef, IrqSource,
    IrqTrigger, MSI_DOMAIN, PendingIrq,
    action::{IrqAction, IrqActionFlags, IrqThreadSlot},
    clear_irq_lifecycle_hooks,
    lifecycle::IrqLifecycleGuard,
    register_irq_lifecycle_hooks,
    softirq::{
        SoftirqRunResult, SoftirqVec, clear_softirq_diagnostics, local_softirq_pending,
        raise_softirq, run_pending_softirqs, softirq_diagnostics,
        test_support::ScopedSoftirqAction,
    },
    state::{IrqLine, MAX_IRQ_ACTIONS},
};

#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to Clippy"
)]
#[allow(
    unknown_lints,
    unregistered_static_spinlock,
    reason = "CG-001-static-spinlock-registration: test-only lock; not tracked in lock_stat"
)]
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
static IRQ_MANAGEMENT_HELD: AtomicBool = AtomicBool::new(false);
static IRQ_MANAGEMENT_SAME_REJECTED: AtomicBool = AtomicBool::new(false);
static IRQ_MANAGEMENT_OTHER_REGISTERED: AtomicBool = AtomicBool::new(false);
static IRQ_MANAGEMENT_METADATA_UNCHANGED: AtomicBool = AtomicBool::new(false);
static IRQ_MANAGEMENT_REMOVAL_REJECTED: AtomicBool = AtomicBool::new(false);

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
const IRQ_MANAGEMENT_TEST_VIRQ: usize = 0x3153;
const IRQ_MANAGEMENT_OTHER_VIRQ: usize = 0x3154;
const IRQ_MANAGEMENT_TEST_HWIRQ: usize = 0x4353;

/// One synthetic in-flight operation on a retained, disabled test Line.
///
/// This unittest-only capability exercises drain accounting, not IRQ delivery.
/// It creates no controller claim, invokes no handler and changes no hardware.
/// Dropping it finishes exactly its own operation and wakes drain waiters after
/// releasing state locks. Keep it alive only across test-controlled handshakes.
pub struct RetainedInFlight {
    virq: usize,
    line: Arc<IrqLine>,
}

impl RetainedInFlight {
    /// Begins accounting on an existing test-owned disabled Line with an action.
    ///
    /// Returns `None` for absent, enabled, empty or already tearing-down Lines.
    /// Call after disabled registration; never use this fixture on a live source.
    pub fn begin(virq: usize) -> Option<Self> {
        let state = IRQ_STATE.lock();
        let line = state.descs.get(&virq)?;
        let mut entry = line.lock();
        if entry.is_enabled_for_tests() || !entry.has_actions() || entry.is_teardown_in_progress() {
            return None;
        }
        entry.begin_dispatch();
        Some(Self {
            virq,
            line: line.clone(),
        })
    }

    /// Observes teardown on the original Line, never a reused IRQ number.
    pub fn is_teardown_in_progress(&self) -> bool {
        self.line.lock().is_teardown_in_progress()
    }
}

impl Drop for RetainedInFlight {
    fn drop(&mut self) {
        let wake = {
            let mut state = IRQ_STATE.lock();
            let wake = self.line.lock().finish_dispatch();
            if state
                .descs
                .get(&self.virq)
                .is_some_and(|line| Arc::ptr_eq(line, &self.line))
            {
                state.remove_if_unused(self.virq);
            }
            wake
        };
        if let Some(wake) = wake {
            wake.wake();
        }
    }
}

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
        .map_or(0, |line| line.lock().in_flight_for_tests());
    IRQ_IN_FLIGHT_OBSERVED.store(in_flight, Ordering::Relaxed);
    IrqEvent::HANDLED
}

struct CountWake(AtomicUsize);

// SAFETY: notification only updates an atomic counter; it cannot allocate,
// unwind, reenter a wait source, or retain any borrowed reference.
unsafe impl SyncWake for CountWake {
    fn notify(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn test_release_in_flight_on_wait(virq: usize, in_flight: usize) {
    if IRQ_WAIT_TEST_TARGET.load(Ordering::Acquire) != virq {
        return;
    }
    // Keep the first observed in-flight count when callbacks race.
    let _ =
        IRQ_WAIT_TEST_OBSERVED.compare_exchange(0, in_flight, Ordering::AcqRel, Ordering::Acquire);
    if IRQ_WAIT_TEST_RELEASES.fetch_add(1, Ordering::AcqRel) == 0 {
        let wait_set = IRQ_STATE
            .lock()
            .descs
            .get_mut(&virq)
            .and_then(|line| line.lock().finish_dispatch());
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
        // A competing callback may have published the first observation; keep it.
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
    let line = Arc::new(IrqLine::new(IrqStateDesc::test_with_runtime(
        IrqDesc::from_virq(IRQ_SIMULATED_TEST_VIRQ),
        Some(IrqAction::regular(Arc::new(test_handler))),
    )));
    let _executor = crate::deferred::ScopedDeferredExecutor::clear();
    let _irq_guard = kspin::NoPreemptIrqSave::new();
    REGULAR_CALLS.store(0, Ordering::Relaxed);
    crate::deferred::clear_deferred_executor();

    {
        let mut state = IRQ_STATE.lock();
        state.descs.insert(IRQ_SIMULATED_TEST_VIRQ, line.clone());
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
    let line = Arc::new(IrqLine::new(IrqStateDesc::new(
        IrqDesc::new(0x2f23, IrqTrigger::EdgeRising).with_virq(virq),
    )));
    {
        let mut state = IRQ_STATE.lock();
        state.descs.insert(virq, line.clone());
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
    let virq = try_map(IrqDesc::new(existing_hwirq, IrqTrigger::EdgeRising).with_domain(domain))
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
    let line = Arc::new(IrqLine::new(IrqStateDesc::test_with_runtime(
        IrqDesc::from_virq(IRQ_LIFECYCLE_TEST_VIRQ),
        Some(IrqAction::regular(Arc::new(test_handler))),
    )));
    let _executor = crate::deferred::ScopedDeferredExecutor::clear();
    let _test_guard = IRQ_LIFECYCLE_TEST_LOCK.lock();
    let _irq_guard = kspin::NoPreemptIrqSave::new();

    clear_irq_lifecycle_hooks();
    crate::deferred::clear_deferred_executor();
    let enter_before = IRQ_ENTER_CALLS.load(Ordering::Relaxed);
    let exit_before = IRQ_EXIT_CALLS.load(Ordering::Relaxed);
    {
        let mut state = IRQ_STATE.lock();
        state.descs.insert(IRQ_LIFECYCLE_TEST_VIRQ, line.clone());
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
    let line = Arc::new(IrqLine::new(IrqStateDesc::test_with_runtime(
        IrqDesc::from_virq(IRQ_TAIL_TEST_VIRQ),
        Some(IrqAction::regular(Arc::new(test_irq_tail_handler))),
    )));
    let _executor = crate::deferred::ScopedDeferredExecutor::clear();
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
        state.descs.insert(IRQ_TAIL_TEST_VIRQ, line.clone());
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
    let _executor = crate::deferred::ScopedDeferredExecutor::clear();
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
fn test_irq_on_empty_result_runs_common_deferred_tail() {
    let _executor = crate::deferred::ScopedDeferredExecutor::clear();
    let _test_guard = IRQ_LIFECYCLE_TEST_LOCK.lock();
    let _irq_guard = kspin::NoPreemptIrqSave::new();

    IRQ_TAIL_UNRESOLVED_DEFERRED.store(0, Ordering::Relaxed);
    crate::deferred::clear_deferred_executor();
    assert!(crate::deferred::register_deferred_executor(
        crate::deferred::DeferredExecutorHooks {
            on_hardirq_exit: Some(test_irq_tail_unresolved_deferred_hook),
        }
    ));

    let hardirq_context_guard = crate::context::HardIrqContextGuard::enter();
    run_irq_exit_tail_with_context(IRQ_TAIL_TEST_VECTOR, None, hardirq_context_guard);

    crate::deferred::clear_deferred_executor();
    assert_eq!(IRQ_TAIL_UNRESOLVED_DEFERRED.load(Ordering::Relaxed), 1);
    assert!(!crate::context::is_in_hardirq());
}

#[def_test(serial)]
fn test_softirq_runs_from_claimed_irq_tail_after_hardirq_exit() {
    let line = Arc::new(IrqLine::new(IrqStateDesc::test_with_runtime(
        IrqDesc::from_virq(IRQ_TAIL_SOFTIRQ_VIRQ),
        Some(IrqAction::regular(Arc::new(
            test_irq_tail_raise_softirq_handler,
        ))),
    )));
    let _executor = crate::deferred::ScopedDeferredExecutor::clear();
    let _test_guard = IRQ_LIFECYCLE_TEST_LOCK.lock();
    let _irq_guard = kspin::NoPreemptIrqSave::new();

    IRQ_TAIL_ORDER_SEQ.store(1, Ordering::Relaxed);
    IRQ_TAIL_HANDLER_ORDER.store(0, Ordering::Relaxed);
    IRQ_TAIL_COMPLETE_ORDER.store(0, Ordering::Relaxed);
    IRQ_TAIL_SOFTIRQ_ACTION_ORDER.store(0, Ordering::Relaxed);
    IRQ_TAIL_SOFTIRQ_IN_HARDIRQ.store(1, Ordering::Relaxed);
    IRQ_TAIL_SOFTIRQ_SERVING.store(0, Ordering::Relaxed);

    crate::deferred::clear_deferred_executor();
    clear_softirq_diagnostics();
    let _softirq_action =
        ScopedSoftirqAction::install(SoftirqVec::High, test_irq_tail_softirq_action);
    {
        let mut state = IRQ_STATE.lock();
        state.descs.insert(IRQ_TAIL_SOFTIRQ_VIRQ, line.clone());
    }

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
fn test_tokenless_free_preserves_shared_action_after_first_slot_removal() {
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
    // The remaining shared action is not in slot zero. Tokenless removal
    // must leave both its ownership and the enabled Line unchanged.
    assert!(try_free_irq(virq).unwrap().is_none());
    assert!(unregister(virq).is_none());
    {
        let state = IRQ_STATE.lock();
        let entry = state
            .descs
            .get(&virq)
            .expect("descriptor must remain")
            .lock();
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
        let entry = state
            .descs
            .get(&virq)
            .expect("descriptor must remain")
            .lock();
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
        let entry = state
            .descs
            .get(&virq)
            .expect("descriptor must remain")
            .lock();
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
        let entry = state
            .descs
            .get(&virq)
            .expect("descriptor must remain")
            .lock();
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

    let line = Arc::new(IrqLine::new(IrqStateDesc::new(initial_desc)));
    {
        let mut state = IRQ_STATE.lock();
        state.descs.insert(virq, line.clone());
        state
            .descs
            .get_mut(&virq)
            .expect("descriptor must exist")
            .lock()
            .begin_teardown();
    }

    assert!(
        !try_register(richer_desc, Arc::new(test_handler))
            .expect("teardown rejection is not a descriptor error")
    );

    {
        let state = IRQ_STATE.lock();
        let entry = state
            .descs
            .get(&virq)
            .expect("descriptor must remain")
            .lock();
        assert_eq!(entry.desc, initial_desc);
        assert_eq!(state.translated_hwirq(GIC_ROOT_DOMAIN, hwirq), None);
    }

    {
        let mut state = IRQ_STATE.lock();
        let mut entry = state
            .descs
            .get_mut(&virq)
            .expect("descriptor must exist")
            .lock();
        entry.finish_teardown();
        drop(entry);
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

    let line = Arc::new(IrqLine::new(IrqStateDesc::new(initial_desc)));
    {
        let mut state = IRQ_STATE.lock();
        state.descs.insert(virq, line.clone());
        state
            .descs
            .get_mut(&virq)
            .expect("descriptor must exist")
            .lock()
            .begin_teardown();
    }

    assert!(
        try_register_shared(richer_desc, Arc::new(test_shared_a_handler))
            .expect("teardown rejection is not a descriptor error")
            .is_none()
    );

    {
        let state = IRQ_STATE.lock();
        let entry = state
            .descs
            .get(&virq)
            .expect("descriptor must remain")
            .lock();
        assert_eq!(entry.desc, initial_desc);
        assert_eq!(entry.action_count(), 0);
        assert_eq!(state.translated_hwirq(GIC_ROOT_DOMAIN, hwirq), None);
    }

    {
        let mut state = IRQ_STATE.lock();
        let mut entry = state
            .descs
            .get_mut(&virq)
            .expect("descriptor must exist")
            .lock();
        entry.finish_teardown();
        drop(entry);
        let _ = state.remove_if_unused(virq);
    }
}

#[def_test(serial)]
fn test_try_register_auto_enable_marks_runtime_enabled() {
    let virq = 0x3110;

    assert!(try_register(virq, Arc::new(test_handler)).expect("register should work"));
    {
        let state = IRQ_STATE.lock();
        let entry = state
            .descs
            .get(&virq)
            .expect("descriptor must exist")
            .lock();
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
        try_register_disabled(virq, Arc::new(test_handler)).expect("disabled register should work")
    );
    {
        let state = IRQ_STATE.lock();
        let entry = state
            .descs
            .get(&virq)
            .expect("descriptor must exist")
            .lock();
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
        try_register_disabled(virq, Arc::new(test_handler)).expect("disabled register should work")
    );
    try_enable_irq(virq).expect("explicit enable should work");
    {
        let state = IRQ_STATE.lock();
        let entry = state
            .descs
            .get(&virq)
            .expect("descriptor must exist")
            .lock();
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
    let mut entry = IrqStateDesc::new(desc);
    assert!(entry.install_regular_action(IrqAction::regular(Arc::new(test_handler))));
    let _ = entry.prepare_auto_enable();
    assert!(entry.take_regular_action().is_some());
    let _ = entry.prepare_disable_if_no_actions();
    entry.begin_teardown();
    let line = Arc::new(IrqLine::new(entry));
    {
        let mut state = IRQ_STATE.lock();
        state.descs.insert(virq, line.clone());
    }

    let err = try_enable_irq(virq).expect_err("teardown enable must fail");
    assert!(matches!(
        err,
        IrqDescError::TeardownInProgress { virq: got } if got == virq
    ));
    {
        let state = IRQ_STATE.lock();
        let entry = state
            .descs
            .get(&virq)
            .expect("descriptor must remain")
            .lock();
        assert!(!entry.is_enabled_for_tests());
        assert_eq!(entry.teardown_depth_for_tests(), 1);
    }

    {
        let mut state = IRQ_STATE.lock();
        let mut entry = state
            .descs
            .get_mut(&virq)
            .expect("descriptor must exist")
            .lock();
        entry.finish_teardown();
        drop(entry);
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

    let mut entry = IrqStateDesc::new(initial_desc);
    entry.begin_teardown();
    let line = Arc::new(IrqLine::new(entry));
    {
        let mut state = IRQ_STATE.lock();
        state.descs.insert(virq, line.clone());
    }

    let err = try_enable(richer_desc, true).expect_err("teardown legacy enable must fail");
    assert!(matches!(
        err,
        IrqDescError::TeardownInProgress { virq: got } if got == virq
    ));
    {
        let state = IRQ_STATE.lock();
        let entry = state
            .descs
            .get(&virq)
            .expect("descriptor must remain")
            .lock();
        assert_eq!(entry.desc, initial_desc);
        assert_eq!(state.translated_hwirq(GIC_ROOT_DOMAIN, hwirq), None);
    }

    {
        let mut state = IRQ_STATE.lock();
        let mut entry = state
            .descs
            .get_mut(&virq)
            .expect("descriptor must exist")
            .lock();
        entry.finish_teardown();
        drop(entry);
        let _ = state.remove_if_unused(virq);
    }
}

#[def_test(serial)]
fn test_enable_irq_rejects_descriptor_without_action() {
    let virq = 0x3128;
    let line = Arc::new(IrqLine::new(IrqStateDesc::new(IrqDesc::from_virq(virq))));
    {
        let mut state = IRQ_STATE.lock();
        state.descs.insert(virq, line.clone());
    }

    let err = try_enable_irq(virq).expect_err("action-less enable must fail");
    assert!(matches!(
        err,
        IrqDescError::NoIrqAction { virq: got } if got == virq
    ));
    {
        let state = IRQ_STATE.lock();
        let entry = state
            .descs
            .get(&virq)
            .expect("descriptor must remain")
            .lock();
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
        let entry = state
            .descs
            .get(&virq)
            .expect("descriptor must exist")
            .lock();
        assert_eq!(entry.disable_depth_for_tests(), 2);
        assert!(!entry.is_enabled_for_tests());
    }

    try_enable_irq(virq).expect("first enable should work");
    {
        let state = IRQ_STATE.lock();
        let entry = state
            .descs
            .get(&virq)
            .expect("descriptor must exist")
            .lock();
        assert_eq!(entry.disable_depth_for_tests(), 1);
        assert!(!entry.is_enabled_for_tests());
    }

    try_enable_irq(virq).expect("second enable should work");
    {
        let state = IRQ_STATE.lock();
        let entry = state
            .descs
            .get(&virq)
            .expect("descriptor must exist")
            .lock();
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
    let line = Arc::new(IrqLine::new(IrqStateDesc::test_with_runtime(
        IrqDesc::from_virq(IRQ_IN_FLIGHT_TEST_VIRQ),
        Some(IrqAction::regular(Arc::new(test_in_flight_handler))),
    )));
    {
        let mut state = IRQ_STATE.lock();
        state.descs.insert(IRQ_IN_FLIGHT_TEST_VIRQ, line.clone());
    }

    let pending = PendingIrq::new(IrqRef::Virq(IRQ_IN_FLIGHT_TEST_VIRQ), 0);
    dispatch_actions(&pending);

    assert_eq!(IRQ_IN_FLIGHT_OBSERVED.load(Ordering::Relaxed), 1);
    {
        let state = IRQ_STATE.lock();
        let entry = state
            .descs
            .get(&IRQ_IN_FLIGHT_TEST_VIRQ)
            .expect("descriptor must remain while action exists")
            .lock();
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
    let first_wake = CountWake(AtomicUsize::new(0));
    let second_wake = CountWake(AtomicUsize::new(0));
    let mut entry = IrqStateDesc::test_with_runtime(
        IrqDesc::from_virq(virq),
        Some(IrqAction::regular(Arc::new(test_handler))),
    );

    entry.begin_dispatch();
    let wait_source = entry.in_flight_zero_completion();
    let mut park_second = || {
        let deferred = entry
            .finish_dispatch()
            .expect("last in-flight dispatch should produce completion wake");
        assert_eq!(first_wake.0.load(Ordering::SeqCst), 0);
        assert_eq!(second_wake.0.load(Ordering::SeqCst), 0);
        assert_eq!(deferred.wake(), 2);
    };
    let mut park_first = || {
        // SAFETY: the nested test callback only finishes dispatch and asserts;
        // it returns or unwinds normally and cannot bypass waiter destruction.
        unsafe { wait_source.wait_with(&second_wake, &mut park_second) };
    };
    // SAFETY: both nested test callbacks return or unwind normally, so scoped
    // cleanup runs before these borrowed counters or their stack can retire.
    unsafe { wait_source.wait_with(&first_wake, &mut park_first) };

    assert_eq!(first_wake.0.load(Ordering::SeqCst), 1);
    assert_eq!(second_wake.0.load(Ordering::SeqCst), 1);
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
        let mut entry = state
            .descs
            .get_mut(&virq)
            .expect("descriptor must exist")
            .lock();
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

fn check_management_isolation_on_wait(virq: usize, in_flight: usize) {
    if virq != IRQ_MANAGEMENT_TEST_VIRQ
        || IRQ_WAIT_TEST_TARGET.load(Ordering::Acquire) != virq
        || IRQ_WAIT_TEST_RELEASES.load(Ordering::Acquire) != 0
    {
        return;
    }
    let line = IRQ_STATE.lock().descs.get(&virq).unwrap().clone();
    let snapshot_fn = || {
        let entry = line.lock();
        (
            entry.desc,
            entry.generation_for_tests(),
            entry.configured_generation_for_tests(),
            entry.is_enabled_for_tests(),
            entry.disable_depth_for_tests(),
            entry.action_count(),
            entry.in_flight(),
            entry.teardown_depth_for_tests(),
        )
    };
    let before = snapshot_fn();
    IRQ_MANAGEMENT_HELD.store(line.management.try_lock().is_none(), Ordering::Release);
    let richer_desc = before
        .0
        .with_controller(IrqController::Gic)
        .with_domain(GIC_ROOT_DOMAIN)
        .with_source(IrqSource::Acpi);
    // Never recursively wait on the teardown's management mutex. The explicit
    // startup path only tries admission and cannot park this drain hook.
    let same = super::try_register_startup(
        richer_desc,
        Arc::new(test_handler),
        super::IrqRegisterOptions::disabled(),
    );
    let other = super::try_register_startup(
        IRQ_MANAGEMENT_OTHER_VIRQ,
        Arc::new(test_handler),
        super::IrqRegisterOptions::disabled(),
    );
    IRQ_MANAGEMENT_SAME_REJECTED.store(matches!(same, Ok(false)), Ordering::Release);
    IRQ_MANAGEMENT_OTHER_REGISTERED.store(matches!(other, Ok(true)), Ordering::Release);
    let has_unchanged_metadata = snapshot_fn() == before
        && IRQ_STATE
            .lock()
            .translated_hwirq(GIC_ROOT_DOMAIN, IRQ_MANAGEMENT_TEST_HWIRQ)
            .is_none();
    IRQ_MANAGEMENT_METADATA_UNCHANGED.store(has_unchanged_metadata, Ordering::Release);
    IRQ_MANAGEMENT_REMOVAL_REJECTED.store(
        IRQ_STATE.lock().remove_if_unused(virq).is_none(),
        Ordering::Release,
    );
    test_release_in_flight_on_wait(virq, in_flight);
}

#[def_test(serial)]
fn test_free_holds_management_per_line_until_dispatch_drains() {
    let virq = IRQ_MANAGEMENT_TEST_VIRQ;
    let desc = IrqDesc::new(IRQ_MANAGEMENT_TEST_HWIRQ, IrqTrigger::Unknown(0)).with_virq(virq);
    {
        let state = IRQ_STATE.lock();
        assert!(!state.descs.contains_key(&virq));
        assert!(!state.descs.contains_key(&IRQ_MANAGEMENT_OTHER_VIRQ));
        assert!(
            state
                .translated_hwirq(GIC_ROOT_DOMAIN, IRQ_MANAGEMENT_TEST_HWIRQ)
                .is_none()
        );
    }
    IRQ_WAIT_TEST_TARGET.store(virq, Ordering::Release);
    IRQ_WAIT_TEST_OBSERVED.store(0, Ordering::Release);
    IRQ_WAIT_TEST_RELEASES.store(0, Ordering::Release);
    IRQ_MANAGEMENT_HELD.store(false, Ordering::Release);
    IRQ_MANAGEMENT_SAME_REJECTED.store(false, Ordering::Release);
    IRQ_MANAGEMENT_OTHER_REGISTERED.store(false, Ordering::Release);
    IRQ_MANAGEMENT_METADATA_UNCHANGED.store(false, Ordering::Release);
    IRQ_MANAGEMENT_REMOVAL_REJECTED.store(false, Ordering::Release);
    assert!(try_register_disabled(desc, Arc::new(test_handler)).unwrap());
    let line = IRQ_STATE.lock().descs.get(&virq).unwrap().clone();
    line.lock().begin_dispatch();
    let old_hook = WAIT_FOR_IN_FLIGHT_HOOK
        .lock()
        .replace(check_management_isolation_on_wait);

    let removed = try_free_irq(virq);

    *WAIT_FOR_IN_FLIGHT_HOOK.lock() = old_hook;
    IRQ_WAIT_TEST_TARGET.store(0, Ordering::Release);
    let was_removed = !IRQ_STATE.lock().descs.contains_key(&virq);
    let registered_after_free = try_register_disabled(desc, Arc::new(test_handler));
    let final_removed = try_free_irq(virq);
    let other_removed = try_free_irq(IRQ_MANAGEMENT_OTHER_VIRQ);
    // Report only after restoring the hook and withdrawing both test actions.
    assert!(removed.unwrap().is_some());
    assert!(was_removed);
    assert_eq!(registered_after_free, Ok(true));
    assert!(final_removed.unwrap().is_some());
    assert!(other_removed.unwrap().is_some());
    assert_eq!(IRQ_WAIT_TEST_OBSERVED.load(Ordering::Acquire), 1);
    assert_eq!(IRQ_WAIT_TEST_RELEASES.load(Ordering::Acquire), 1);
    assert!(IRQ_MANAGEMENT_HELD.load(Ordering::Acquire));
    assert!(IRQ_MANAGEMENT_SAME_REJECTED.load(Ordering::Acquire));
    assert!(IRQ_MANAGEMENT_OTHER_REGISTERED.load(Ordering::Acquire));
    assert!(IRQ_MANAGEMENT_METADATA_UNCHANGED.load(Ordering::Acquire));
    assert!(IRQ_MANAGEMENT_REMOVAL_REJECTED.load(Ordering::Acquire));
}

fn replace_drained_instance_on_wait(virq: usize, _in_flight: usize) {
    if IRQ_WAIT_TEST_TARGET.load(Ordering::Acquire) != virq {
        return;
    }
    let pass = IRQ_WAIT_TEST_RELEASES.fetch_add(1, Ordering::AcqRel);
    let replacement = (pass == 0).then(|| {
        let mut entry = IrqStateDesc::test_with_runtime(
            IrqDesc::from_virq(virq),
            Some(IrqAction::regular(Arc::new(test_handler))),
        );
        entry.begin_dispatch();
        Arc::new(IrqLine::new(entry))
    });
    let (wake, removed_action) = {
        let mut state = IRQ_STATE.lock();
        let mut entry = state
            .descs
            .get_mut(&virq)
            .expect("waiting instance exists")
            .lock();
        let wake = entry
            .finish_dispatch()
            .expect("one synthetic dispatch remains");
        let removed_action = if pass == 0 {
            let action = entry.take_regular_action();
            drop(entry);
            assert!(state.remove_if_unused(virq).is_some());
            assert!(
                state
                    .descs
                    .insert(virq, replacement.as_ref().unwrap().clone())
                    .is_none()
            );
            action
        } else {
            // A regressed waiter reaches the replacement. Finish it so this
            // test fails an assertion instead of hanging the entire runner.
            None
        };
        (wake, removed_action)
    };
    wake.wake();
    drop(removed_action);
}

fn check_wait_keeps_original_instance(wait_fn: impl FnOnce(usize) -> Result<bool, IrqDescError>) {
    let virq = 0x3152;
    assert!(try_register(virq, Arc::new(test_handler)).unwrap());
    IRQ_STATE
        .lock()
        .descs
        .get_mut(&virq)
        .unwrap()
        .lock()
        .begin_dispatch();
    IRQ_WAIT_TEST_TARGET.store(virq, Ordering::Release);
    IRQ_WAIT_TEST_RELEASES.store(0, Ordering::Release);
    let old_hook = WAIT_FOR_IN_FLIGHT_HOOK
        .lock()
        .replace(replace_drained_instance_on_wait);

    let result = wait_fn(virq);

    *WAIT_FOR_IN_FLIGHT_HOOK.lock() = old_hook;
    IRQ_WAIT_TEST_TARGET.store(0, Ordering::Release);
    let passes = IRQ_WAIT_TEST_RELEASES.load(Ordering::Acquire);
    let (replacement_in_flight, wake) = {
        let mut state = IRQ_STATE.lock();
        let mut entry = state.descs.get_mut(&virq).unwrap().lock();
        let in_flight = entry.in_flight();
        let wake = (in_flight != 0).then(|| entry.finish_dispatch().unwrap());
        (in_flight, wake)
    };
    if let Some(wake) = wake {
        wake.wake();
    }
    assert!(unregister(virq).is_some());

    assert_eq!(result, Ok(true));
    assert_eq!(passes, 1, "wait must not follow the reused IRQ number");
    assert_eq!(
        replacement_in_flight, 1,
        "replacement was not part of this wait"
    );
}

#[def_test(serial)]
fn test_synchronize_irq_does_not_follow_reused_number() {
    check_wait_keeps_original_instance(try_synchronize_irq);
}

#[def_test(serial)]
fn test_disable_irq_does_not_follow_reused_number() {
    check_wait_keeps_original_instance(try_disable_irq);
}

#[def_test(serial)]
fn test_irq_drain_retains_line_after_directory_removal() {
    let virq = 0x3153;
    let line = Arc::new(IrqLine::new(IrqStateDesc::new(IrqDesc::from_virq(virq))));
    let weak = Arc::downgrade(&line);
    let (drain, removed) = {
        let mut state = IRQ_STATE.lock();
        assert!(!state.descs.contains_key(&virq));
        state.descs.insert(virq, line.clone());
        let drain = super::IrqDrain::new(virq, state.descs.get(&virq).unwrap());
        let removed = state.remove_if_unused(virq);
        (drain, removed)
    };
    drop(line);

    assert!(removed.is_some());
    assert!(
        weak.upgrade().is_some(),
        "drain must retain the withdrawn Line"
    );
    drain.wait();
    assert!(
        weak.upgrade().is_some(),
        "waiting must not release drain ownership"
    );
    drop(drain);
    assert!(
        weak.upgrade().is_none(),
        "the final drain owner must release the Line"
    );
}

#[def_test(serial)]
fn test_disable_irq_waits_after_nosync_disable() {
    let virq = 0x3116;

    assert!(try_register(virq, Arc::new(test_handler)).expect("register should work"));
    assert!(try_disable_irq(virq).expect("disable_irq should not fail"));
    {
        let state = IRQ_STATE.lock();
        let entry = state
            .descs
            .get(&virq)
            .expect("descriptor must exist")
            .lock();
        assert_eq!(entry.disable_depth_for_tests(), 1);
        assert!(!entry.is_enabled_for_tests());
    }

    assert!(unregister(virq).is_some());
}

#[def_test]
fn test_synchronize_irq_rejects_hardirq_context() {
    let _hardirq = crate::context::test_support::ScopedHardIrqContext::enter();

    let err = try_synchronize_irq(0x3117).expect_err("context misuse must be an error");
    assert!(matches!(
        err,
        IrqDescError::InvalidContext {
            operation: "synchronize_irq"
        }
    ));
}

#[def_test(serial)]
fn test_free_irq_rejects_masked_irqs_before_action_removal() {
    let virq = 0x3150;
    assert!(try_register(virq, Arc::new(test_handler)).expect("register should work"));
    let rejected = {
        let _guard = kspin::NoPreemptIrqSave::new();
        super::try_free_irq(virq)
    };
    assert!(matches!(
        rejected,
        Err(IrqDescError::InvalidContext {
            operation: "free_irq"
        })
    ));
    {
        let state = IRQ_STATE.lock();
        let entry = state
            .descs
            .get(&virq)
            .expect("action must remain installed")
            .lock();
        assert_eq!(entry.action_count(), 1);
        assert!(!entry.is_teardown_in_progress());
    }
    assert!(unregister(virq).is_some());
}

#[def_test(serial)]
fn test_free_irq_action_rejects_hardirq_context() {
    let _hardirq = crate::context::test_support::ScopedHardIrqContext::enter();

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
        let mut entry = state
            .descs
            .get_mut(&virq)
            .expect("descriptor must exist")
            .lock();
        entry.begin_dispatch();
    }

    assert!(try_synchronize_irq(virq).expect("synchronize_irq should not fail"));

    *WAIT_FOR_IN_FLIGHT_HOOK.lock() = old_hook;
    IRQ_WAIT_TEST_TARGET.store(0, Ordering::Release);
    assert_eq!(IRQ_WAIT_TEST_OBSERVED.load(Ordering::Acquire), 1);
    assert_eq!(IRQ_WAIT_TEST_RELEASES.load(Ordering::Acquire), 1);
    {
        let state = IRQ_STATE.lock();
        let entry = state
            .descs
            .get(&virq)
            .expect("descriptor must remain")
            .lock();
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

    let line = Arc::new(IrqLine::new(IrqStateDesc::test_with_runtime(
        IrqDesc::from_virq(virq),
        Some(IrqAction::regular(Arc::new(test_not_handled_handler))),
    )));
    {
        let mut state = IRQ_STATE.lock();
        state.descs.insert(virq, line.clone());
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

    let line = Arc::new(IrqLine::new(IrqStateDesc::test_with_runtime(
        IrqDesc::from_virq(virq),
        Some(IrqAction::test_new(
            Arc::new(test_handler),
            Some(IrqThreadSlot),
            IrqActionFlags::NO_THREAD,
            Some("future-threaded"),
        )),
    )));
    {
        let mut state = IRQ_STATE.lock();
        state.descs.insert(virq, line.clone());
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
            .lock()
            .has_actions()
    );
    assert!(unregister(virq).is_some());
}

#[def_test(serial)]
fn test_unregister_keeps_msi_descriptor_for_free_msix() {
    let (hwirq, virq) =
        unused_msi_test_mapping(&IRQ_STATE.lock()).expect("MSI test mapping space exhausted");
    let desc = IrqDesc::new(hwirq, IrqTrigger::EdgeRising)
        .with_domain(MSI_DOMAIN)
        .with_flags(IrqFlags::MSI)
        .with_virq(virq);
    let prepared = Arc::new(IrqLine::new(IrqStateDesc::new(desc)));
    let action = IrqAction::regular(Arc::new(test_handler));
    {
        let mut state = IRQ_STATE.lock();
        assert!(unused_msi_test_hwirq(&state, hwirq));
        assert!(!state.descs.contains_key(&virq));
        let desc = state
            .try_resolve_desc(desc, Some(&prepared))
            .expect("MSI descriptor should be accepted")
            .into_desc();
        let mut entry = state
            .descs
            .get_mut(&desc.logical_irq().unwrap())
            .expect("descriptor state must exist after try_resolve_desc")
            .lock();
        assert!(entry.install_regular_action(action));
    }

    assert!(unregister(virq).is_some());
    {
        let state = IRQ_STATE.lock();
        let entry = state
            .descs
            .get(&virq)
            .expect("MSI descriptor must stay")
            .lock();
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

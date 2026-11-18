// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kcompletion::SyncWake;
use unittest::def_test;

use super::{
    DYNAMIC_VIRQ_BASE, IRQ_STATE, IrqDesc, IrqDescError, IrqDomainId, IrqStateDesc,
    WAIT_FOR_RUNNING_HOOK, run_claimed_irq_tail, run_irq_exit_tail_with_context, try_disable_irq,
    try_disable_irq_nosync, try_enable, try_enable_irq, try_free_irq, try_free_irq_action, try_map,
    try_register, try_register_disabled, try_register_shared, try_synchronize_irq, unregister,
};
use crate::{
    GIC_ROOT_DOMAIN, IrqController, IrqEvent, IrqFlags, IrqLifecycleHooks, IrqRef, IrqSource,
    IrqTrigger, MSI_DOMAIN, PendingIrq,
    action::{IrqAction, IrqActionFlags, IrqThreadSlot},
    clear_irq_lifecycle_hooks,
    lifecycle::IrqLifecycleGuard,
    register_irq_lifecycle_hooks,
    softirq::{
        SoftirqVec, local_softirq_pending, raise_softirq, run_pending_softirqs,
        test_support::ScopedSoftirqAction,
    },
    state::IrqLine,
};
pub use crate::{runtime::nmi::NmiShutdownProbe, state::RcuIndexProbe};

/// An explicitly bound, software-only ordinary Line for lifecycle tests.
pub struct SyntheticLine {
    virq: usize,
    line: Arc<IrqLine>,
}

impl SyntheticLine {
    /// Publishes a fake-flow candidate through the real mapping transaction.
    pub fn mapped(desc: IrqDesc) -> Self {
        let line = Arc::new(IrqLine::new(IrqStateDesc::test_new(desc)));
        let mapped = IRQ_STATE
            .lock()
            .try_resolve_desc(desc, Some(&line))
            .unwrap();
        let virq = mapped.into_desc().logical_irq().unwrap();
        assert!(Arc::ptr_eq(
            &IRQ_STATE.lock().descs.get(&virq).unwrap(),
            &line
        ));
        Self { virq, line }
    }

    /// Installs a synthetic immediate affinity callback for procfs fixtures.
    pub fn with_affinity_chip(&self, chip: &'static crate::IrqAffinityChip) {
        self.line.lock().test_affinity_chip = Some(chip);
    }

    /// Installs a synthetic deferred affinity callback for procfs fixtures.
    pub fn with_deferred_affinity_chip(
        &self,
        chip: &'static crate::IrqDeferredAffinityChip,
    ) {
        self.line.lock().test_deferred_affinity = Some(chip);
    }

    pub fn virq(&self) -> usize {
        self.virq
    }

    /// Establishes a requested policy without publishing an action.
    pub fn set_requested_policy_for_tests(&self, mask: kcpu_id_map::KCpuMask) {
        self.line.lock().affinity.requested = Some(mask);
    }

    pub fn clear_action_history_for_tests(&self) {
        self.line.lock().clear_action_history_for_tests();
    }

    /// Runs the same retired-list handoff used by a real Line retirement.
    pub fn retire_for_tests(&self) {
        let retirements = {
            let mut line = self.line.lock();
            line.set_retired_for_tests();
            line.take_retirements()
        };
        crate::runtime::retirement::notify_retirements(self.virq, retirements);
        let removed = {
            let mut registry = IRQ_STATE.lock();
            if registry
                .descs
                .get(&self.virq)
                .is_some_and(|line| Arc::ptr_eq(&line, &self.line))
            {
                registry.remove_test_line(self.virq)
            } else {
                None
            }
        };
        drop(removed);
    }

    /// The caller exclusively owns this vacant numeric slot for the scope.
    pub fn new(desc: IrqDesc) -> Self {
        let virq = desc.logical_irq().expect("fixture needs an explicit virq");
        let line = Arc::new(IrqLine::new(IrqStateDesc::test_new(desc)));
        {
            let mut registry = IRQ_STATE.lock();
            assert!(!registry.descs.contains_key(&virq));
            registry.descs.insert(virq, line.clone());
        }
        line.lock().set_action_history_for_tests();
        Self { virq, line }
    }
}

impl Drop for SyntheticLine {
    fn drop(&mut self) {
        let removed = {
            let mut registry = IRQ_STATE.lock();
            if registry
                .descs
                .get(&self.virq)
                .is_some_and(|line| Arc::ptr_eq(&line, &self.line))
            {
                registry.remove_test_line(self.virq)
            } else {
                None
            }
        };
        drop(removed);
    }
}

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
static IRQ_TAIL_SOFTIRQ_CPU: AtomicUsize = AtomicUsize::new(usize::MAX);
static IRQ_TAIL_SOFTIRQ_CALLS: AtomicUsize = AtomicUsize::new(0);
static IRQ_TAIL_OTHER_SOFTIRQ_CALLS: AtomicUsize = AtomicUsize::new(0);
static IRQ_TAIL_UNRESOLVED_DEFERRED: AtomicUsize = AtomicUsize::new(0);
static IRQ_IN_FLIGHT_OBSERVED: AtomicUsize = AtomicUsize::new(0);
static IRQ_WAIT_TEST_TARGET: AtomicUsize = AtomicUsize::new(0);
static IRQ_WAIT_TEST_OBSERVED: AtomicUsize = AtomicUsize::new(0);
static IRQ_WAIT_TEST_RELEASES: AtomicUsize = AtomicUsize::new(0);
static IRQ_WAIT_OWNER_DROPS: AtomicUsize = AtomicUsize::new(0);
static IRQ_WAIT_OWNER_RETAINED: AtomicBool = AtomicBool::new(false);
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

#[def_test]
fn local_action_withdrawal_requires_all_recorded_slots_disabled() {
    let _pin = kspin::NoPreemptIrqSave::new();
    let virq = 0x5f84;
    let line = Arc::new(IrqLine::new(IrqStateDesc::test_new(IrqDesc::from_virq(
        virq,
    ))));
    {
        let mut entry = line.lock();
        entry.set_flow_for_tests(crate::IrqFlow::PerCpu);
        assert!(entry.install_regular_action(IrqAction::regular(Arc::new(|_| IrqEvent::HANDLED))));
        let plan = entry.prepare_auto_enable();
        entry.commit_platform_plan(plan);
    }
    let rejected = super::withdraw_action_on_line(virq, &line, None, true, |_| {
        panic!("rejected local removal must not program hardware");
    })
    .unwrap();
    assert!(rejected.is_none());
    {
        let mut entry = line.lock();
        assert_eq!(entry.action_count(), 1);
        assert!(!entry.is_teardown_in_progress());
        let plan = entry.prepare_disable_irq_nosync();
        entry.commit_platform_plan(plan);
    }
    let removed = super::withdraw_action_on_line(virq, &line, None, true, |_| {
        panic!("PerCpu removal must not apply controller operations");
    })
    .unwrap();
    assert!(removed.is_some());
    assert_eq!(line.lock().action_count(), 0);
    // SAFETY: this private fixture has no admitted reader or controller claim.
    let removed = unsafe { removed.unwrap().reclaim() };
    drop(removed);
}

#[def_test]
fn ordinary_disabled_registration_masks_preexisting_enable() {
    let virq = 0x5f87;
    let handler: crate::Handler = Arc::new(test_handler);
    let line = Arc::new(IrqLine::new(IrqStateDesc::test_new(IrqDesc::from_virq(
        virq,
    ))));
    let mut writes = Vec::with_capacity(2);
    {
        let mut entry = line.lock();
        let enable = entry.prepare_legacy_enable();
        assert_eq!(enable.enable, Some(true));
        writes.push(true);
        entry.commit_platform_plan(enable);
        assert!(entry.is_enabled_for_tests());
        assert!(
            super::register_locked(
                virq,
                &mut entry,
                &handler,
                super::IrqRegisterOptions::disabled(),
                |plan| {
                    if let Some(enabled) = plan.enable {
                        writes.push(enabled);
                    }
                    Ok(())
                },
            )
            .unwrap()
        );
        assert!(!entry.is_enabled_for_tests());
        assert_eq!(entry.disable_depth_for_tests(), 1);
        assert!(!entry.can_run_actions());
        let disable = entry.prepare_disable_irq_nosync();
        assert_eq!(disable.enable, None);
        entry.commit_platform_plan(disable);
        assert_eq!(entry.disable_depth_for_tests(), 2);
    }
    assert_eq!(writes.as_slice(), &[true, false]);
    let retired = super::withdraw_action_on_line(virq, &line, None, false, |plan| {
        assert_eq!(plan.enable, None);
    })
    .unwrap()
    .unwrap();
    super::IrqDrain::new(virq, &line).wait();
    assert_eq!(line.lock().action_count(), 0);
    // SAFETY: this private Line has no reader; its execution drain completed.
    drop(unsafe { retired.reclaim() });
    assert_eq!(Arc::strong_count(&handler), 1);
}

#[def_test]
fn private_local_registration_plan_masks_preexisting_enable() {
    let virq = 0x5f86;
    let handler: crate::Handler = Arc::new(test_handler);
    let line = Arc::new(IrqLine::new(IrqStateDesc::test_new(IrqDesc::from_virq(
        virq,
    ))));
    let mut writes = Vec::with_capacity(3);
    {
        // Exercise the low-level plan, not public registration admission: the
        // dedicated public API rejects an enabled slot before reaching this.
        // The recording backend never touches hardware.
        let mut entry = line.lock();
        entry.set_flow_for_tests(crate::IrqFlow::PerCpu);
        let enable = entry.prepare_legacy_enable();
        assert_eq!(enable.enable, Some(true));
        writes.push(true);
        entry.commit_platform_plan(enable);
        assert!(entry.has_enabled_local_cpu());
        assert!(
            super::register_locked(
                virq,
                &mut entry,
                &handler,
                super::IrqRegisterOptions::disabled(),
                |plan| {
                    assert!(!karch::local_irq_enabled());
                    if let Some(enabled) = plan.enable {
                        writes.push(enabled);
                    }
                    Ok(())
                },
            )
            .unwrap()
        );
        assert_eq!(entry.action_count(), 1);
        assert!(!entry.has_enabled_local_cpu());
        assert!(!entry.can_run_local_actions());
        let disable = entry.prepare_disable_irq_nosync();
        assert_eq!(disable.enable, Some(false));
        writes.push(false);
        entry.commit_platform_plan(disable);
    }
    assert_eq!(writes.as_slice(), &[true, false, false]);
    let removed = super::withdraw_action_on_line(virq, &line, None, true, |_| {
        panic!("PerCpu removal must not apply controller operations");
    })
    .unwrap();
    assert!(removed.is_some());
    super::IrqDrain::new(virq, &line).wait();
    assert_eq!(line.lock().action_count(), 0);
    // SAFETY: the private fixture has no readers; its original Line is drained.
    let removed = unsafe { removed.unwrap().reclaim() };
    drop(removed);
    assert_eq!(Arc::strong_count(&handler), 1);
}

/// Retires an exclusively test-owned, synthetic domainless descriptor.
///
/// Call only after joining all test users and withdrawing their actions. The
/// source must never have been connected to hardware. This is fixture cleanup,
/// not a production mapping-disposal API; busy or enabled Lines are rejected.
pub fn retire_unused_test_line(virq: usize) -> bool {
    let removed = {
        let mut state = IRQ_STATE.lock();
        let Some(line) = state.descs.get(&virq) else {
            return false;
        };
        let entry = line.lock();
        if entry.desc.domain.is_some()
            || entry.is_msi()
            || !entry.is_unused()
            || entry.is_enabled_for_tests()
        {
            return false;
        }
        drop(entry);
        state.descs.remove(&virq)
    };
    // Even fixture storage must be finally destroyed outside the registry lock.
    removed.is_some()
}

/// Removes both indexes for an exclusively reserved ordinary hardware fixture.
///
/// The fixture owns the source independently of mapping vacancy. It must stop
/// injection, mask and drain the controller, remove its action, and exclude all
/// other mapping users before calling. This test-only cleanup is not a dynamic
/// production mapping lease. MSI, private and NMI sources are not supported.
pub fn retire_reserved_test_line(virq: usize) -> bool {
    let removed = {
        let mut state = IRQ_STATE.lock();
        let Some(line) = state.descs.get(&virq) else {
            return false;
        };
        let entry = line.lock();
        if entry.is_msi()
            || entry.flow == Some(crate::IrqFlow::PerCpu)
            || entry.has_nmi_action()
            || entry.has_nmi_shutdown()
            || !entry.is_unused()
            || entry.is_enabled_for_tests()
        {
            return false;
        }
        drop(entry);
        state.remove_test_line(virq)
    };
    // This uses quiescent fixture removal, not GP-delayed retirement: the
    // caller has excluded all borrowed readers of this key. Destroy the
    // returned owner outside both short locks.
    removed.is_some()
}

/// Synthetic local flow fixture; never performs controller accesses.
pub struct LocalFlowProbe {
    virq: usize,
    line: Arc<IrqLine>,
}

impl LocalFlowProbe {
    /// Installs a synthetic Line owned exclusively by the test.
    pub fn new(virq: usize, handler: Arc<dyn crate::IrqHandler>) -> Self {
        let line = Arc::new(IrqLine::new(IrqStateDesc::test_new(IrqDesc::from_virq(
            virq,
        ))));
        {
            let mut state = IRQ_STATE.lock();
            core::assert!(!state.descs.contains_key(&virq));
            let mut entry = line.lock();
            entry.set_flow_for_tests(crate::IrqFlow::PerCpu);
            core::assert!(entry.install_regular_action(IrqAction::regular(handler)));
            state.descs.insert(virq, line.clone());
        }
        Self { virq, line }
    }

    /// Changes only this CPU's synthetic software control state.
    pub fn set_enabled(&self, enabled: bool) {
        let mut entry = self.line.lock();
        let plan = if enabled {
            entry.prepare_enable_irq()
        } else {
            entry.prepare_disable_irq_nosync()
        };
        entry.commit_platform_plan(plan);
    }

    /// Runs the real generic flow, substituting only claim completion.
    pub fn dispatch(&self) -> bool {
        let mut completed = false;
        crate::dispatch::dispatch_claim(crate::IrqRef::Virq(self.virq), || completed = true);
        completed
    }
}

impl Drop for LocalFlowProbe {
    fn drop(&mut self) {
        let mut state = IRQ_STATE.lock();
        let entry = self.line.lock();
        core::assert!(!entry.has_enabled_local_cpu());
        state.descs.remove(&self.virq);
    }
}

/// One synthetic running dispatch on a retained, disabled ordinary test Line.
///
/// This unittest-only capability exercises drain accounting, not IRQ delivery.
/// It creates no controller claim, invokes no handler and changes no hardware.
/// Dropping it finishes exactly its own operation and wakes drain waiters after
/// releasing state locks. Keep it alive only across test-controlled handshakes.
pub struct RetainedRunningDispatch {
    virq: usize,
    line: Arc<IrqLine>,
}

impl RetainedRunningDispatch {
    /// Begins accounting on an existing test-owned disabled Line with an action.
    ///
    /// Returns `None` for unbound/PerCpu, absent, enabled, empty, running or
    /// already tearing-down Lines.
    /// Call after disabled registration; never use this fixture on a live source.
    pub fn begin(virq: usize) -> Option<Self> {
        let state = IRQ_STATE.lock();
        let line = state.descs.get(&virq)?;
        let mut entry = line.lock();
        if !matches!(entry.flow, Some(flow) if flow != crate::IrqFlow::PerCpu)
            || entry.is_running
            || entry.is_enabled_for_tests()
            || !entry.has_actions()
            || entry.is_teardown_in_progress()
        {
            return None;
        }
        entry.begin_running_dispatch();
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

impl Drop for RetainedRunningDispatch {
    fn drop(&mut self) {
        let wake = {
            let mut state = IRQ_STATE.lock();
            let wake = self.line.lock().finish_running_dispatch();
            if state
                .descs
                .get(&self.virq)
                .is_some_and(|line| Arc::ptr_eq(&line, &self.line))
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

fn test_running_handler(_irq: usize) -> IrqEvent {
    let active = IRQ_STATE
        .lock()
        .descs
        .get(&IRQ_IN_FLIGHT_TEST_VIRQ)
        .map_or(0, |line| usize::from(line.lock().has_active_execution()));
    IRQ_IN_FLIGHT_OBSERVED.store(active, Ordering::Relaxed);
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

fn test_release_running_on_wait(virq: usize, active: usize) {
    if IRQ_WAIT_TEST_TARGET.load(Ordering::Acquire) != virq {
        return;
    }
    // Keep the first observed running diagnostic when callbacks race.
    let _ = IRQ_WAIT_TEST_OBSERVED.compare_exchange(0, active, Ordering::AcqRel, Ordering::Acquire);
    if IRQ_WAIT_TEST_RELEASES.fetch_add(1, Ordering::AcqRel) == 0 {
        let wait_set = IRQ_STATE
            .lock()
            .descs
            .get(&virq)
            .and_then(|line| line.lock().finish_running_dispatch());
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
    raise_softirq(SoftirqVec::IrqPoll);
    // An unrelated vector must not change the oracle for the IrqPoll callback.
    raise_softirq(SoftirqVec::Hrtimer);
    IrqEvent::HANDLED
}

fn test_irq_tail_softirq_action() {
    if kiface_cpu_api::CurrentCpuIf::current_cpu().as_usize()
        != IRQ_TAIL_SOFTIRQ_CPU.load(Ordering::Acquire)
    {
        return;
    }
    IRQ_TAIL_SOFTIRQ_CALLS.fetch_add(1, Ordering::Relaxed);
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

fn test_irq_tail_other_softirq_action() {
    if kiface_cpu_api::CurrentCpuIf::current_cpu().as_usize()
        == IRQ_TAIL_SOFTIRQ_CPU.load(Ordering::Acquire)
    {
        IRQ_TAIL_OTHER_SOFTIRQ_CALLS.fetch_add(1, Ordering::Relaxed);
    }
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
    state
        .translated_hwirq(crate::domain::TEST_DOMAIN, hwirq)
        .is_none()
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
    {
        let mut entry = line.lock();
        let plan = entry.prepare_auto_enable();
        entry.commit_platform_plan(plan);
    }
    let _executor = crate::deferred::ScopedDeferredExecutor::clear();
    let _irq_guard = kspin::NoPreemptIrqSave::new();
    REGULAR_CALLS.store(0, Ordering::Relaxed);
    crate::deferred::clear_deferred_executor();

    {
        let mut state = IRQ_STATE.lock();
        state.descs.insert(IRQ_SIMULATED_TEST_VIRQ, line.clone());
    }

    let pending = PendingIrq::simulated(IrqRef::Virq(IRQ_SIMULATED_TEST_VIRQ));
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
    let domain = crate::domain::TEST_DOMAIN;
    let hwirq = 0x2f11;
    let first = IrqDesc::new(hwirq, IrqTrigger::EdgeRising).with_domain(domain);
    let binding = SyntheticLine::mapped(first);
    let virq = binding.virq();
    let conflicting_virq = virq + 1;
    let conflict = IrqDesc::new(hwirq, IrqTrigger::EdgeRising)
        .with_domain(domain)
        .with_virq(conflicting_virq);
    // Supply an explicit software binding so resolution reaches the mapping
    // conflict rather than asking a real controller to accept this test key.
    let alternate_desc = IrqDesc::from_hwirq(hwirq).with_virq(conflicting_virq);
    let alternate = SyntheticLine::new(alternate_desc);

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
    assert_eq!(alternate.line.lock().desc, alternate_desc);
    assert_eq!(alternate.line.lock().action_count(), 0);
    {
        let registry = IRQ_STATE.lock();
        assert!(Arc::ptr_eq(
            &registry.descs.get(&conflicting_virq).unwrap(),
            &alternate.line
        ));
        assert_eq!(registry.translated_hwirq(domain, hwirq), Some(virq));
    }
    let removed = { IRQ_STATE.lock().remove_test_line(virq) };
    drop(removed);
}

#[def_test(serial)]
fn test_try_map_rolls_back_domain_mapping_on_desc_conflict() {
    let domain = crate::domain::TEST_DOMAIN;
    let hwirq = 0x2f21;
    let virq = 0x2f22;
    let line = Arc::new(IrqLine::new(IrqStateDesc::test_new(
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
    let domain = crate::domain::TEST_DOMAIN;
    let existing_hwirq = 0x2f2a;
    let newer_hwirq = 0x2f2b;
    let binding = SyntheticLine::mapped(
        IrqDesc::new(existing_hwirq, IrqTrigger::EdgeRising).with_domain(domain),
    );
    let virq = binding.virq();
    // A published mapping retains both indexes until paired withdrawal. Keep
    // that invariant while attempting to alias its logical number.

    let err = try_map(
        IrqDesc::new(newer_hwirq, IrqTrigger::EdgeRising)
            .with_domain(domain)
            .with_virq(virq),
    )
    .expect_err("virq already targeted by another mapping must fail");

    assert!(matches!(
        err,
        IrqDescError::HwirqConflict { existing, newer }
            if existing == existing_hwirq && newer == newer_hwirq
    ));
    assert_eq!(IRQ_STATE.lock().translated_hwirq(domain, newer_hwirq), None);
}

#[def_test(serial)]
fn test_try_map_rejects_dynamic_virq_exhaustion_without_wrap() {
    let domain = crate::domain::TEST_DOMAIN;
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
    let mut state_desc = IrqStateDesc::test_new(desc);
    let initial_generation = state_desc.generation_for_tests();

    state_desc.update_desc(desc);
    assert_eq!(state_desc.generation_for_tests(), initial_generation);

    state_desc.update_desc(desc.with_flags(IrqFlags::MSI));
    assert!(state_desc.generation_for_tests() > initial_generation);
}

#[def_test]
fn test_teardown_gate_blocks_new_actions_until_finished() {
    let desc = IrqDesc::from_virq(0x2f2e);
    let mut state_desc = IrqStateDesc::test_new(desc);

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
    let mut state_desc = IrqStateDesc::test_new(desc);
    let token_a = state_desc
        .install_shared_action(Arc::new(test_shared_a_handler))
        .expect("first shared action");
    let token_b = state_desc
        .install_shared_action(Arc::new(test_shared_b_handler))
        .expect("second shared action");
    let plan = state_desc.prepare_auto_enable();
    state_desc.commit_platform_plan(plan);

    assert!(super::take_from_state(&mut state_desc, Some(token_a)).is_some());
    assert!(state_desc.prepare_disable_if_no_actions().is_none());
    state_desc.begin_teardown();
    assert!(super::take_from_state(&mut state_desc, Some(token_b)).is_some());

    let plan = state_desc
        .prepare_disable_if_no_actions()
        .expect("last action should mask line even during teardown wait");
    assert_eq!(plan.enable, Some(false));
    assert!(state_desc.is_enabled_for_tests());
    state_desc.commit_platform_plan(plan);
    assert!(!state_desc.is_enabled_for_tests());
    assert_eq!(state_desc.teardown_depth_for_tests(), 1);
}

#[def_test(serial)]
fn test_irq_lifecycle_hooks_fire_around_normal_irq_handler() {
    let line = Arc::new(IrqLine::new(IrqStateDesc::test_with_runtime(
        IrqDesc::from_virq(IRQ_LIFECYCLE_TEST_VIRQ),
        Some(IrqAction::regular(Arc::new(test_handler))),
    )));
    {
        let mut entry = line.lock();
        let plan = entry.prepare_auto_enable();
        entry.commit_platform_plan(plan);
    }
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
        let pending = PendingIrq::simulated(IrqRef::Virq(IRQ_LIFECYCLE_TEST_VIRQ));
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
    {
        let mut entry = line.lock();
        let plan = entry.prepare_auto_enable();
        entry.commit_platform_plan(plan);
    }
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
        let pending = PendingIrq::simulated(IrqRef::Virq(IRQ_TAIL_TEST_VIRQ));
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
    let pending = PendingIrq::simulated(IrqRef::Domain(MSI_DOMAIN, unresolved_hwirq));
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
    {
        let mut entry = line.lock();
        let plan = entry.prepare_auto_enable();
        entry.commit_platform_plan(plan);
    }
    let _executor = crate::deferred::ScopedDeferredExecutor::clear();
    let _test_guard = IRQ_LIFECYCLE_TEST_LOCK.lock();
    let _irq_guard = kspin::NoPreemptIrqSave::new();

    IRQ_TAIL_ORDER_SEQ.store(1, Ordering::Relaxed);
    IRQ_TAIL_HANDLER_ORDER.store(0, Ordering::Relaxed);
    IRQ_TAIL_COMPLETE_ORDER.store(0, Ordering::Relaxed);
    IRQ_TAIL_SOFTIRQ_ACTION_ORDER.store(0, Ordering::Relaxed);
    IRQ_TAIL_SOFTIRQ_IN_HARDIRQ.store(1, Ordering::Relaxed);
    IRQ_TAIL_SOFTIRQ_SERVING.store(0, Ordering::Relaxed);
    IRQ_TAIL_SOFTIRQ_CALLS.store(0, Ordering::Relaxed);
    IRQ_TAIL_OTHER_SOFTIRQ_CALLS.store(0, Ordering::Relaxed);
    IRQ_TAIL_SOFTIRQ_CPU.store(
        kiface_cpu_api::CurrentCpuIf::current_cpu().as_usize(),
        Ordering::Release,
    );

    crate::deferred::clear_deferred_executor();
    // Do not replace live workqueue, RCU or device-service callbacks.
    assert!(!crate::softirq::is_softirq_open(SoftirqVec::IrqPoll));
    assert!(!crate::softirq::is_softirq_open(SoftirqVec::Hrtimer));
    let _softirq_action =
        ScopedSoftirqAction::install(SoftirqVec::IrqPoll, test_irq_tail_softirq_action);
    let _other_softirq_action =
        ScopedSoftirqAction::install(SoftirqVec::Hrtimer, test_irq_tail_other_softirq_action);
    {
        let mut state = IRQ_STATE.lock();
        state.descs.insert(IRQ_TAIL_SOFTIRQ_VIRQ, line.clone());
    }

    assert!(crate::softirq::init());

    let pending = PendingIrq::simulated(IrqRef::Virq(IRQ_TAIL_SOFTIRQ_VIRQ));
    run_claimed_irq_tail(IRQ_TAIL_TEST_VECTOR, pending, |pending| {
        test_irq_tail_complete();
        pending.complete();
    });

    let handler_order = IRQ_TAIL_HANDLER_ORDER.load(Ordering::Relaxed);
    let complete_order = IRQ_TAIL_COMPLETE_ORDER.load(Ordering::Relaxed);
    let softirq_order = IRQ_TAIL_SOFTIRQ_ACTION_ORDER.load(Ordering::Relaxed);
    let softirq_in_hardirq = IRQ_TAIL_SOFTIRQ_IN_HARDIRQ.load(Ordering::Relaxed);
    let softirq_serving = IRQ_TAIL_SOFTIRQ_SERVING.load(Ordering::Relaxed);
    // Serial tests do not stop background services on this or other CPUs.
    // Observe only our callbacks and vectors, never aggregate diagnostics.
    let owned_mask = (1 << SoftirqVec::IrqPoll.as_usize()) | (1 << SoftirqVec::Hrtimer.as_usize());
    let pending = local_softirq_pending() & owned_mask;
    let calls = IRQ_TAIL_SOFTIRQ_CALLS.load(Ordering::Relaxed);
    let other_calls = IRQ_TAIL_OTHER_SOFTIRQ_CALLS.load(Ordering::Relaxed);
    let _ = run_pending_softirqs();
    let calls_after_drain = IRQ_TAIL_SOFTIRQ_CALLS.load(Ordering::Relaxed);
    let other_calls_after_drain = IRQ_TAIL_OTHER_SOFTIRQ_CALLS.load(Ordering::Relaxed);
    IRQ_TAIL_SOFTIRQ_CPU.store(usize::MAX, Ordering::Release);

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
    assert_eq!(calls, 1);
    assert_eq!(other_calls, 1);
    assert_eq!(calls_after_drain, calls);
    assert_eq!(other_calls_after_drain, other_calls);
}

#[def_test(serial)]
fn normal_claim_consumption_rejects_effective_nmi_context() {
    let virq = 0x3101;
    let _binding = SyntheticLine::new(IrqDesc::from_virq(virq));
    REGULAR_CALLS.store(0, Ordering::Relaxed);
    assert!(try_register(virq, Arc::new(test_handler)).unwrap());
    {
        let _pin = kspin::NoPreemptIrqSave::new();
        // SAFETY: initialized test CPU, pinned with ordinary IRQs masked until
        // the effective NMI scope is dropped. No hardware claim is fabricated.
        let _nmi = unsafe { crate::context::NmiContextGuard::enter() };
        assert_eq!(
            crate::generic_handle_irq(PendingIrq::simulated(IrqRef::Virq(virq))),
            None
        );
        assert_eq!(REGULAR_CALLS.load(Ordering::Relaxed), 0);
    }
    super::dispatch_simulated_irq(PendingIrq::simulated(IrqRef::Virq(virq)));
    assert_eq!(REGULAR_CALLS.load(Ordering::Relaxed), 1);
    assert!(unregister(virq).is_some());
    assert!(retire_unused_test_line(virq));
}

#[def_test(serial)]
fn test_duplicate_regular_handler_registration_is_rejected() {
    let virq = 0x3101;
    let _binding = SyntheticLine::new(IrqDesc::from_virq(virq));
    assert_eq!(try_map(IrqDesc::from_virq(virq)), Ok(virq));
    REGULAR_CALLS.store(0, Ordering::Relaxed);

    assert!(try_register(virq, Arc::new(test_handler)).expect("first register should work"));
    assert!(
        !try_register(virq, Arc::new(test_not_handled_handler))
            .expect("duplicate register should report compatibility false")
    );

    let pending = PendingIrq::simulated(IrqRef::Virq(virq));
    super::dispatch_simulated_irq(pending);

    assert_eq!(REGULAR_CALLS.load(Ordering::Relaxed), 1);
    assert!(unregister(virq).is_some());
    assert!(retire_unused_test_line(virq));
}

#[def_test(serial)]
fn test_shared_irq_fanout_runs_all_actions() {
    let virq = 0x3120;
    let _binding = SyntheticLine::new(IrqDesc::from_virq(virq));
    assert_eq!(try_map(IrqDesc::from_virq(virq)), Ok(virq));
    SHARED_A_CALLS.store(0, Ordering::Relaxed);
    SHARED_B_CALLS.store(0, Ordering::Relaxed);

    let token_a = try_register_shared(virq, Arc::new(test_shared_a_handler))
        .expect("first shared register should work")
        .expect("first shared action token");
    let token_b = try_register_shared(virq, Arc::new(test_shared_b_handler))
        .expect("second shared register should work")
        .expect("second shared action token");

    let pending = PendingIrq::simulated(IrqRef::Virq(virq));
    super::dispatch_simulated_irq(pending);

    assert_eq!(SHARED_A_CALLS.load(Ordering::Relaxed), 1);
    assert_eq!(SHARED_B_CALLS.load(Ordering::Relaxed), 1);
    assert!(try_free_irq_action(virq, token_a).unwrap().is_some());
    assert!(try_free_irq_action(virq, token_b).unwrap().is_some());
    assert!(retire_unused_test_line(virq));
}

#[def_test(serial)]
fn test_tokenless_free_preserves_shared_action_after_first_slot_removal() {
    let virq = 0x3121;
    let _binding = SyntheticLine::new(IrqDesc::from_virq(virq));
    assert_eq!(try_map(IrqDesc::from_virq(virq)), Ok(virq));
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
        let retained_line = state.descs.get(&virq).expect("descriptor must remain");
        let entry = retained_line.lock();
        assert_eq!(entry.action_count(), 1);
        assert!(entry.is_enabled_for_tests());
    }

    let pending = PendingIrq::simulated(IrqRef::Virq(virq));
    super::dispatch_simulated_irq(pending);

    assert_eq!(SHARED_A_CALLS.load(Ordering::Relaxed), 0);
    assert_eq!(SHARED_B_CALLS.load(Ordering::Relaxed), 1);
    assert!(try_free_irq_action(virq, token_b).unwrap().is_some());
    assert!(IRQ_STATE.lock().remove_if_unused(virq).is_some());
}

#[def_test(serial)]
fn shared_irq_fanout_exceeds_four_and_runs_every_handler() {
    const FANOUT: usize = 17;
    let virq = 0x312c;
    let _binding = SyntheticLine::new(IrqDesc::from_virq(virq));
    assert_eq!(try_map(IrqDesc::from_virq(virq)), Ok(virq));
    SHARED_A_CALLS.store(0, Ordering::Relaxed);

    let mut tokens = Vec::new();
    for _ in 0..FANOUT {
        let token = try_register_shared(virq, Arc::new(test_shared_a_handler))
            .expect("shared register should not fail")
            .expect("compatible shared action should install");
        tokens.push(token);
    }

    {
        let state = IRQ_STATE.lock();
        let retained_line = state.descs.get(&virq).expect("descriptor must remain");
        let entry = retained_line.lock();
        assert_eq!(entry.action_count(), FANOUT);
    }

    let pending = PendingIrq::simulated(IrqRef::Virq(virq));
    super::dispatch_simulated_irq(pending);
    assert_eq!(SHARED_A_CALLS.load(Ordering::Relaxed), FANOUT);

    for token in tokens {
        assert!(try_free_irq_action(virq, token).unwrap().is_some());
    }
    assert!(IRQ_STATE.lock().remove_if_unused(virq).is_some());
}

#[def_test(serial)]
fn test_free_irq_rejects_single_shared_action_without_token() {
    let virq = 0x312d;
    let _binding = SyntheticLine::new(IrqDesc::from_virq(virq));
    assert_eq!(try_map(IrqDesc::from_virq(virq)), Ok(virq));

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
        let retained_line = state.descs.get(&virq).expect("descriptor must remain");
        let entry = retained_line.lock();
        assert_eq!(entry.action_count(), 1);
    }

    assert!(try_free_irq_action(virq, token).unwrap().is_some());
    assert!(retire_unused_test_line(virq));
}

#[def_test(serial)]
fn test_free_irq_action_rejects_forged_regular_token() {
    let virq = 0x3125;
    let _binding = SyntheticLine::new(IrqDesc::from_virq(virq));
    assert_eq!(try_map(IrqDesc::from_virq(virq)), Ok(virq));

    assert!(try_register(virq, Arc::new(test_handler)).expect("regular register should work"));
    assert!(
        try_free_irq_action(virq, crate::IrqActionToken::new(0))
            .expect("forged token rejection is not a descriptor error")
            .is_none()
    );
    {
        let state = IRQ_STATE.lock();
        let retained_line = state.descs.get(&virq).expect("descriptor must remain");
        let entry = retained_line.lock();
        assert_eq!(entry.action_count(), 1);
    }

    assert!(unregister(virq).is_some());
    assert!(retire_unused_test_line(virq));
}

#[def_test(serial)]
fn test_regular_and_shared_registration_do_not_mix() {
    let virq = 0x3123;
    let _binding = SyntheticLine::new(IrqDesc::from_virq(virq));
    assert_eq!(try_map(IrqDesc::from_virq(virq)), Ok(virq));

    assert!(try_register(virq, Arc::new(test_handler)).expect("regular register should work"));
    assert!(
        try_register_shared(virq, Arc::new(test_shared_a_handler))
            .expect("mixed shared request is a policy rejection")
            .is_none()
    );
    assert!(unregister(virq).is_some());
    assert!(retire_unused_test_line(virq));
}

#[cfg(target_arch = "aarch64")]
#[def_test(serial)]
fn registration_rejects_newly_resolved_percpu_and_retains_metadata() {
    for is_shared in [false, true] {
        let virq = DYNAMIC_VIRQ_BASE + 0x5fca;
        let initial = IrqDesc::new(15, IrqTrigger::Unknown(0)).with_virq(virq);
        let refined = initial.with_source(IrqSource::Acpi);
        let line = Arc::new(IrqLine::new(IrqStateDesc::new(initial)));
        let mut registry = super::IrqState::new();
        let handler: crate::Handler = Arc::new(test_handler);
        let mut action = if is_shared {
            super::PreparedAction::shared(handler.clone())
        } else {
            super::PreparedAction::regular(handler.clone())
        }
        .unwrap();
        let mut publication =
            crate::runtime::index::PreparedPublication::new(virq, None, true).unwrap();
        let resolution = registry.plan_resolution(refined.into()).unwrap();
        let mut applied = 0;
        {
            let mut entry = line.lock();
            // This is an unpublished constructor-default candidate, not a
            // previously installed binding with its selected flow overwritten.
            assert_eq!(entry.flow, None);
            assert!(super::require_non_percpu_flow(&entry).is_err());
            registry
                .commit_resolution(resolution, &line, &mut entry, &mut publication)
                .unwrap();
            assert_eq!(entry.flow, Some(crate::IrqFlow::PerCpu));
            assert_eq!(entry.desc, refined);
            let generation = entry.generation_for_tests();
            let depth = entry.disable_depth_for_tests();
            let apply = |_| {
                applied += 1;
                Ok(())
            };
            let result = if is_shared {
                super::register_shared_prepared_locked(virq, &mut entry, &mut action, apply)
                    .map(|token| token.is_some())
            } else {
                // Mirror the production final-binding check, then invoke the
                // real publisher only if it accepts the resolved binding.
                super::RegistrationKind::Regular(super::IrqRegisterOptions::auto_enable())
                    .options(&entry)
                    .and_then(|options| {
                        super::register_prepared_locked(
                            virq,
                            &mut entry,
                            &mut action,
                            options.unwrap(),
                            apply,
                        )
                    })
            };
            assert_eq!(result, Err(IrqDescError::UnsupportedFlow { hwirq: 15 }));
            assert_eq!(applied, 0);
            assert_eq!(entry.desc, refined);
            assert_eq!(entry.generation_for_tests(), generation);
            assert_eq!(entry.disable_depth_for_tests(), depth);
            assert_eq!(entry.action_count(), 0);
            assert!(!entry.is_running);
            assert!(!entry.has_enabled_local_cpu());
            assert!(entry.prepare_reconfigure_if_stale().configure);
            assert!(!entry.needs_configuration_restore());
        }
        // No source is configured or enabled; no global directory or domain
        // mapping was published. Offered ownership is released outside locks.
        drop(action);
        assert_eq!(Arc::strong_count(&handler), 1);
        assert!(Arc::ptr_eq(&registry.descs.get(&virq).unwrap(), &line));
    }
}

#[def_test(serial)]
fn unbound_candidate_rejects_activation_and_completes_claim_once() {
    let virq = DYNAMIC_VIRQ_BASE + 0x5fcb;
    let desc = IrqDesc::from_virq(virq);
    let line = Arc::new(IrqLine::new(IrqStateDesc::new(desc)));
    let handler: crate::Handler = Arc::new(|_| panic!("unbound action ran"));
    let mut regular = super::PreparedAction::regular(handler.clone()).unwrap();
    let mut shared = super::PreparedAction::shared(handler.clone()).unwrap();
    {
        let mut entry = line.lock();
        assert_eq!(entry.flow, None);
        assert_eq!(
            super::register_prepared_locked(
                virq,
                &mut entry,
                &mut regular,
                super::IrqRegisterOptions::auto_enable(),
                |_| panic!("unbound registration reached hardware"),
            ),
            Err(IrqDescError::UnsupportedFlow { hwirq: virq })
        );
        assert!(matches!(
            super::register_shared_prepared_locked(
                virq, &mut entry, &mut shared,
                |_| panic!("unbound shared registration reached hardware"),
            ),
            Err(IrqDescError::UnsupportedFlow { hwirq }) if hwirq == virq
        ));
        for force in [false, true] {
            assert!(matches!(
                super::prepare_enable_plan(&entry, virq, force),
                Err(IrqDescError::UnsupportedFlow { hwirq }) if hwirq == virq
            ));
        }
        assert_eq!(entry.action_count(), 0);
        assert_eq!(entry.disable_depth_for_tests(), 0);
        assert_eq!(entry.configured_generation_for_tests(), None);
    }
    drop(regular);
    drop(shared);
    assert_eq!(Arc::strong_count(&handler), 1);
    let idle = line.lock().execution_idle_completion();
    let has_atomic_exclusion = {
        let _guard = line.lock();
        line.is_state_locked_for_tests()
    };
    // Publish only into the synthetic directory to exercise rejected lookup;
    // no controller mapping or action is installed for this unbound candidate.
    assert!(IRQ_STATE.lock().descs.insert(virq, line.clone()).is_none());
    let cleanup = SyntheticLine {
        virq,
        line: line.clone(),
    };
    let mut completions = 0;
    assert_eq!(
        crate::dispatch::dispatch_claim(IrqRef::Virq(virq), || {
            completions += 1;
            assert!(!karch::local_irq_enabled());
            assert!(!has_atomic_exclusion || line.is_state_locked_for_tests());
            assert!(idle.is_completed());
        }),
        Some(virq)
    );
    assert_eq!(completions, 1);
    assert_eq!(line.lock().flow, None);
    assert!(!line.lock().is_running);
    assert_eq!(line.lock().action_count(), 0);
    drop(cleanup);
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

    let line = Arc::new(IrqLine::new(IrqStateDesc::test_new(initial_desc)));
    {
        let mut state = IRQ_STATE.lock();
        state.descs.insert(virq, line.clone());
        state
            .descs
            .get(&virq)
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
        let retained_line = state.descs.get(&virq).expect("descriptor must remain");
        let entry = retained_line.lock();
        assert_eq!(entry.desc, initial_desc);
        assert_eq!(state.translated_hwirq(GIC_ROOT_DOMAIN, hwirq), None);
    }

    {
        let mut state = IRQ_STATE.lock();
        let retained_line = state.descs.get(&virq).expect("descriptor must exist");
        let mut entry = retained_line.lock();
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

    let line = Arc::new(IrqLine::new(IrqStateDesc::test_new(initial_desc)));
    {
        let mut state = IRQ_STATE.lock();
        state.descs.insert(virq, line.clone());
        state
            .descs
            .get(&virq)
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
        let retained_line = state.descs.get(&virq).expect("descriptor must remain");
        let entry = retained_line.lock();
        assert_eq!(entry.desc, initial_desc);
        assert_eq!(entry.action_count(), 0);
        assert_eq!(state.translated_hwirq(GIC_ROOT_DOMAIN, hwirq), None);
    }

    {
        let mut state = IRQ_STATE.lock();
        let retained_line = state.descs.get(&virq).expect("descriptor must exist");
        let mut entry = retained_line.lock();
        entry.finish_teardown();
        drop(entry);
        let _ = state.remove_if_unused(virq);
    }
}

#[def_test(serial)]
fn test_try_register_auto_enable_marks_runtime_enabled() {
    let virq = 0x3110;
    let _binding = SyntheticLine::new(IrqDesc::from_virq(virq));
    assert_eq!(try_map(IrqDesc::from_virq(virq)), Ok(virq));

    assert!(try_register(virq, Arc::new(test_handler)).expect("register should work"));
    {
        let state = IRQ_STATE.lock();
        let retained_line = state.descs.get(&virq).expect("descriptor must exist");
        let entry = retained_line.lock();
        assert_eq!(entry.disable_depth_for_tests(), 0);
        assert!(entry.is_enabled_for_tests());
        assert_eq!(
            entry.configured_generation_for_tests(),
            Some(entry.generation_for_tests())
        );
    }

    assert!(unregister(virq).is_some());
    assert!(retire_unused_test_line(virq));
}

#[def_test(serial)]
fn test_try_register_disabled_leaves_runtime_disabled() {
    let virq = 0x3111;
    let _binding = SyntheticLine::new(IrqDesc::from_virq(virq));
    assert_eq!(try_map(IrqDesc::from_virq(virq)), Ok(virq));

    assert!(
        try_register_disabled(virq, Arc::new(test_handler)).expect("disabled register should work")
    );
    {
        let state = IRQ_STATE.lock();
        let retained_line = state.descs.get(&virq).expect("descriptor must exist");
        let entry = retained_line.lock();
        assert_eq!(entry.disable_depth_for_tests(), 1);
        assert!(!entry.is_enabled_for_tests());
        assert_eq!(
            entry.configured_generation_for_tests(),
            Some(entry.generation_for_tests())
        );
    }

    assert!(unregister(virq).is_some());
    assert!(retire_unused_test_line(virq));
}

#[def_test(serial)]
fn test_enable_irq_starts_disabled_registration() {
    let virq = 0x3112;
    let _binding = SyntheticLine::new(IrqDesc::from_virq(virq));
    assert_eq!(try_map(IrqDesc::from_virq(virq)), Ok(virq));

    assert!(
        try_register_disabled(virq, Arc::new(test_handler)).expect("disabled register should work")
    );
    try_enable_irq(virq).expect("explicit enable should work");
    {
        let state = IRQ_STATE.lock();
        let retained_line = state.descs.get(&virq).expect("descriptor must exist");
        let entry = retained_line.lock();
        assert_eq!(entry.disable_depth_for_tests(), 0);
        assert!(entry.is_enabled_for_tests());
        assert_eq!(
            entry.configured_generation_for_tests(),
            Some(entry.generation_for_tests())
        );
    }

    assert!(unregister(virq).is_some());
    assert!(retire_unused_test_line(virq));
}

#[def_test(serial)]
fn test_enable_irq_rejects_teardown_wait_window() {
    let virq = 0x3127;
    let desc = IrqDesc::from_virq(virq);
    let mut entry = IrqStateDesc::test_new(desc);
    assert!(entry.install_regular_action(IrqAction::regular(Arc::new(test_handler))));
    let plan = entry.prepare_auto_enable();
    entry.commit_platform_plan(plan);
    assert!(super::take_from_state(&mut entry, None).is_some());
    if let Some(plan) = entry.prepare_disable_if_no_actions() {
        entry.commit_platform_plan(plan);
    }
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
        let retained_line = state.descs.get(&virq).expect("descriptor must remain");
        let entry = retained_line.lock();
        assert!(!entry.is_enabled_for_tests());
        assert_eq!(entry.teardown_depth_for_tests(), 1);
    }

    {
        let mut state = IRQ_STATE.lock();
        let retained_line = state.descs.get(&virq).expect("descriptor must exist");
        let mut entry = retained_line.lock();
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

    let mut entry = IrqStateDesc::test_new(initial_desc);
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
        let retained_line = state.descs.get(&virq).expect("descriptor must remain");
        let entry = retained_line.lock();
        assert_eq!(entry.desc, initial_desc);
        assert_eq!(state.translated_hwirq(GIC_ROOT_DOMAIN, hwirq), None);
    }

    {
        let mut state = IRQ_STATE.lock();
        let retained_line = state.descs.get(&virq).expect("descriptor must exist");
        let mut entry = retained_line.lock();
        entry.finish_teardown();
        drop(entry);
        let _ = state.remove_if_unused(virq);
    }
}

#[def_test(serial)]
fn test_enable_irq_rejects_descriptor_without_action() {
    let virq = 0x3128;
    let line = Arc::new(IrqLine::new(IrqStateDesc::test_new(IrqDesc::from_virq(
        virq,
    ))));
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
        let retained_line = state.descs.get(&virq).expect("descriptor must remain");
        let entry = retained_line.lock();
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
    let _binding = SyntheticLine::new(IrqDesc::from_virq(virq));
    assert_eq!(try_map(IrqDesc::from_virq(virq)), Ok(virq));

    assert!(try_register(virq, Arc::new(test_handler)).expect("register should work"));
    try_disable_irq_nosync(virq).expect("first disable should work");
    try_disable_irq_nosync(virq).expect("nested disable should work");
    {
        let state = IRQ_STATE.lock();
        let retained_line = state.descs.get(&virq).expect("descriptor must exist");
        let entry = retained_line.lock();
        assert_eq!(entry.disable_depth_for_tests(), 2);
        assert!(!entry.is_enabled_for_tests());
    }

    try_enable_irq(virq).expect("first enable should work");
    {
        let state = IRQ_STATE.lock();
        let retained_line = state.descs.get(&virq).expect("descriptor must exist");
        let entry = retained_line.lock();
        assert_eq!(entry.disable_depth_for_tests(), 1);
        assert!(!entry.is_enabled_for_tests());
    }

    try_enable_irq(virq).expect("second enable should work");
    {
        let state = IRQ_STATE.lock();
        let retained_line = state.descs.get(&virq).expect("descriptor must exist");
        let entry = retained_line.lock();
        assert_eq!(entry.disable_depth_for_tests(), 0);
        assert!(entry.is_enabled_for_tests());
    }

    assert!(unregister(virq).is_some());
    assert!(retire_unused_test_line(virq));
}

#[def_test(serial)]
fn test_disable_irq_nosync_rejects_unknown_virq_without_creating_descriptor() {
    let virq = 0x3119;

    assert!(!IRQ_STATE.lock().descs.contains_key(&virq));
    let err = try_disable_irq_nosync(virq).expect_err("unknown virq must fail");

    assert!(matches!(err, IrqDescError::UnknownIrq));
    assert!(!IRQ_STATE.lock().descs.contains_key(&virq));
}

#[def_test]
fn enable_final_admission_rechecks_teardown() {
    let virq = 0x3163;
    for force in [false, true] {
        let mut entry = IrqStateDesc::test_new(IrqDesc::from_virq(virq));
        assert!(entry.install_regular_action(IrqAction::regular(Arc::new(test_handler))));
        let plan = entry.prepare_register_disabled();
        entry.commit_platform_plan(plan);
        assert!(!entry.is_teardown_in_progress());
        // Model a stale observation without relying on another CPU's timing.
        entry.begin_teardown();
        let result = super::prepare_enable_plan(&entry, virq, force);
        assert!(matches!(result, Err(IrqDescError::TeardownInProgress { virq: id }) if id == virq));
        assert_eq!(entry.disable_depth_for_tests(), 1);
        assert!(!entry.is_enabled_for_tests());
        entry.finish_teardown();
        assert_eq!(
            super::prepare_enable_plan(&entry, virq, force)
                .unwrap()
                .enable,
            Some(true)
        );
    }
}

#[def_test(serial)]
fn control_transactions_preserve_already_masked_context() {
    let registry = kspin::SpinNoIrq::new(super::IrqState::new());
    let virq = 0x3164;
    let mut entry = IrqStateDesc::test_new(IrqDesc::from_virq(virq));
    assert!(entry.install_regular_action(IrqAction::regular(Arc::new(test_handler))));
    let line = Arc::new(IrqLine::new(entry));
    registry.lock().descs.insert(virq, line);
    let irq_before = karch::local_irq_enabled();
    {
        let _context = kspin::NoPreemptIrqSave::new();
        super::enable_line(&registry, virq.into(), false, |_| Ok(())).unwrap();
        assert!(!karch::local_irq_enabled());
        let _drain = super::disable_line(&registry, virq.into(), false, |_| {}).unwrap();
        assert!(!karch::local_irq_enabled());
        let mut has_applied = false;
        let result = super::enable_line(&registry, (virq + 1).into(), false, |_| {
            has_applied = true;
            Ok(())
        });
        assert!(matches!(result, Err(IrqDescError::UnknownIrq)));
        assert!(!has_applied);
        assert!(!karch::local_irq_enabled());
    }
    assert_eq!(karch::local_irq_enabled(), irq_before);
}

#[def_test]
fn disabled_registration_configures_before_publication_and_enable_reuses_configuration() {
    let virq = 0x315e;
    let handler: crate::Handler = Arc::new(test_handler);
    let mut entry = IrqStateDesc::test_new(IrqDesc::from_virq(virq));
    let mut observed = Vec::new();
    assert!(
        super::register_locked(
            virq,
            &mut entry,
            &handler,
            super::IrqRegisterOptions::disabled(),
            |plan| {
                observed.push((
                    plan.configure,
                    plan.enable,
                    plan.flow_unmask.is_some(),
                    Arc::strong_count(&handler),
                ));
                Ok(())
            },
        )
        .unwrap()
    );
    assert_eq!(
        observed.as_slice(),
        &[(true, None, false, 2), (false, None, false, 2)]
    );
    assert_eq!(entry.disable_depth_for_tests(), 1);
    assert!(!entry.is_enabled_for_tests());
    assert_eq!(
        entry.configured_generation_for_tests(),
        Some(entry.generation_for_tests())
    );

    let enable = entry.prepare_enable_irq();
    assert!(!enable.configure);
    assert_eq!(enable.enable, Some(true));
    entry.commit_platform_plan(enable);
    assert_eq!(entry.disable_depth_for_tests(), 0);
    assert!(entry.is_enabled_for_tests());
    assert_eq!(entry.action_count(), 1);
}

#[def_test]
fn configuration_failure_leaves_regular_and_disabled_registration_unowned() {
    let virq = 0x3170;
    let handler: crate::Handler = Arc::new(test_handler);
    for is_masked in [false, true] {
        let irq_before = karch::local_irq_enabled();
        let irq_guard = is_masked.then(kspin::IrqSave::new);
        let expected_irq = karch::local_irq_enabled();
        for options in [
            super::IrqRegisterOptions::auto_enable(),
            super::IrqRegisterOptions::disabled(),
        ] {
            let line = IrqLine::new(IrqStateDesc::test_new(IrqDesc::from_virq(virq)));
            let mut observed = Vec::new();
            let result =
                super::register_locked(virq, &mut line.lock(), &handler, options, |plan| {
                    observed.push((
                        plan.configure,
                        plan.enable,
                        plan.flow_unmask.is_some(),
                        Arc::strong_count(&handler),
                        karch::local_irq_enabled(),
                    ));
                    Err(IrqDescError::ConfigurationFailed {
                        hwirq: plan.desc.hwirq,
                    })
                });
            assert_eq!(
                result,
                Err(IrqDescError::ConfigurationFailed { hwirq: virq })
            );
            assert_eq!(observed.as_slice(), &[(true, None, false, 2, false)]);
            assert_eq!(karch::local_irq_enabled(), expected_irq);
            let entry = line.lock();
            assert_eq!(entry.action_count(), 0);
            assert_eq!(entry.configured_generation_for_tests(), None);
            assert_eq!(entry.disable_depth_for_tests(), 0);
            assert!(!entry.is_enabled_for_tests());
            assert_eq!(Arc::strong_count(&handler), 1);
        }
        drop(irq_guard);
        assert_eq!(karch::local_irq_enabled(), irq_before);
    }
}

#[def_test]
fn restricted_configuration_failure_publishes_no_policy() {
    let virq = 0x3173;
    let line = IrqLine::new(IrqStateDesc::test_new(IrqDesc::from_virq(virq)));
    let handler: crate::Handler = Arc::new(test_handler);
    let result = super::register_locked(
        virq,
        &mut line.lock(),
        &handler,
        super::IrqRegisterOptions::disabled().restrict_user_affinity(),
        |plan| {
            Err(IrqDescError::ConfigurationFailed {
                hwirq: plan.desc.hwirq,
            })
        },
    );
    assert_eq!(
        result,
        Err(IrqDescError::ConfigurationFailed { hwirq: virq })
    );
    let entry = line.lock();
    assert_eq!(entry.action_count(), 0);
    assert!(!entry.has_user_affinity_restriction());
    assert_eq!(Arc::strong_count(&handler), 1);
}

#[def_test]
fn shared_configuration_failure_preserves_existing_owners_and_tokens() {
    let virq = 0x3171;
    let owner: crate::Handler = Arc::new(test_handler);
    let offered: crate::Handler = Arc::new(test_handler);
    for has_owner in [false, true] {
        for is_disabled in [false, true] {
            let mut entry = IrqStateDesc::test_new(IrqDesc::from_virq(virq));
            let token = if has_owner {
                super::register_shared_locked(virq, &mut entry, &owner, |_| Ok(())).unwrap()
            } else {
                None
            };
            if is_disabled {
                let plan = entry.prepare_disable_irq_nosync();
                entry.commit_platform_plan(plan);
            }
            entry.update_desc(entry.desc.with_flags(IrqFlags::WAKEUP_SOURCE));
            let configured_before = entry.configured_generation_for_tests();
            let depth_before = entry.disable_depth_for_tests();
            let enabled_before = entry.is_enabled_for_tests();
            let generation = entry.generation_for_tests();
            let line = IrqLine::new(entry);
            let irq_before = karch::local_irq_enabled();
            let mut observed = Vec::new();
            let result = super::register_shared_locked(virq, &mut line.lock(), &offered, |plan| {
                observed.push((plan.configure, plan.enable, plan.flow_unmask.is_some()));
                Err(IrqDescError::ConfigurationFailed {
                    hwirq: plan.desc.hwirq,
                })
            });
            assert_eq!(
                result,
                Err(IrqDescError::ConfigurationFailed { hwirq: virq })
            );
            assert_eq!(observed.as_slice(), &[(true, None, false)]);
            assert_eq!(karch::local_irq_enabled(), irq_before);
            let entry = line.lock();
            assert_eq!(entry.action_count(), usize::from(has_owner));
            assert_eq!(entry.configured_generation_for_tests(), configured_before);
            assert_eq!(entry.generation_for_tests(), generation);
            assert_eq!(entry.disable_depth_for_tests(), depth_before);
            assert_eq!(entry.is_enabled_for_tests(), enabled_before);
            assert_eq!(Arc::strong_count(&offered), 1);
            assert_eq!(Arc::strong_count(&owner), 1 + usize::from(has_owner));
            drop(entry);
            if let Some(token) = token {
                assert!(super::take_from_line(&line, Some(token)).is_some());
            }
            assert_eq!(Arc::strong_count(&owner), 1);
        }
    }
}

#[def_test]
fn enable_configuration_failure_preserves_depth_generation_pending_and_irq_mask() {
    let virq = 0x3172;
    let owner: crate::Handler = Arc::new(test_handler);
    for force in [false, true] {
        for is_masked in [false, true] {
            let mut entry = IrqStateDesc::test_new(IrqDesc::from_virq(virq));
            assert!(
                super::register_locked(
                    virq,
                    &mut entry,
                    &owner,
                    super::IrqRegisterOptions::disabled(),
                    |_| Ok(()),
                )
                .unwrap()
            );
            entry.update_desc(entry.desc.with_flags(IrqFlags::WAKEUP_SOURCE));
            entry.is_pending = true;
            let configured_before = entry.configured_generation_for_tests();
            let generation = entry.generation_for_tests();
            let line = Arc::new(IrqLine::new(entry));
            let registry = kspin::SpinNoIrq::new(super::IrqState::new());
            registry.lock().descs.insert(virq, line.clone());
            let irq_before = karch::local_irq_enabled();
            let irq_guard = is_masked.then(kspin::IrqSave::new);
            let expected_irq = karch::local_irq_enabled();
            let mut observed = None;
            let result = super::enable_line(&registry, virq.into(), force, |plan| {
                observed = Some((plan.configure, plan.enable, karch::local_irq_enabled()));
                Err(IrqDescError::ConfigurationFailed {
                    hwirq: plan.desc.hwirq,
                })
            });
            assert_eq!(
                result,
                Err(IrqDescError::ConfigurationFailed { hwirq: virq })
            );
            assert_eq!(observed, Some((true, Some(true), false)));
            assert_eq!(karch::local_irq_enabled(), expected_irq);
            drop(irq_guard);
            assert_eq!(karch::local_irq_enabled(), irq_before);
            let entry = line.lock();
            assert_eq!(entry.configured_generation_for_tests(), configured_before);
            assert_eq!(entry.generation_for_tests(), generation);
            assert_eq!(entry.disable_depth_for_tests(), 1);
            assert!(!entry.is_enabled_for_tests());
            assert!(entry.is_pending);
            assert!(!entry.is_running);
            assert_eq!(entry.action_count(), 1);
            assert_eq!(Arc::strong_count(&owner), 2);
        }
    }
    assert_eq!(Arc::strong_count(&owner), 1);
}

#[def_test]
fn failed_shared_configuration_blocks_survivor_unmask_until_successful_retry() {
    static CHIP: crate::FasteoiChip = crate::FasteoiChip {
        mask: |_| {},
        unmask: |_| {},
        resend: crate::FasteoiResend::Software,
    };
    let virq = 0x3173;
    let owner: crate::Handler = Arc::new(test_handler);
    let offered: crate::Handler = Arc::new(test_handler);
    let mut entry = IrqStateDesc::test_new(IrqDesc::from_virq(virq));
    entry.set_flow_for_tests(crate::IrqFlow::Fasteoi(&CHIP));
    let survivor = super::register_shared_locked(virq, &mut entry, &owner, |_| Ok(()))
        .unwrap()
        .unwrap();
    let withdrawn = super::register_shared_locked(virq, &mut entry, &owner, |_| Ok(()))
        .unwrap()
        .unwrap();
    entry.update_desc(entry.desc.with_flags(IrqFlags::WAKEUP_SOURCE));
    entry.is_pending = true;
    let configured_before = entry.configured_generation_for_tests();
    let result = super::register_shared_locked(virq, &mut entry, &offered, |plan| {
        assert!(plan.configure);
        Err(IrqDescError::ConfigurationFailed { hwirq: virq })
    });
    assert_eq!(
        result,
        Err(IrqDescError::ConfigurationFailed { hwirq: virq })
    );
    assert!(!entry.can_run_actions());
    assert!(!entry.needs_replay());
    assert!(entry.needs_configuration_restore());
    entry.begin_management();
    let line = Arc::new(IrqLine::new(entry));
    let registry = kspin::SpinNoIrq::new(super::IrqState::new());
    registry.lock().descs.insert(virq, line.clone());
    let mut has_applied_delivery = false;
    let removed = super::withdraw_on_line(virq, &line, Some(withdrawn), |_| {
        has_applied_delivery = true;
    });
    assert!(removed.is_some());
    assert!(!has_applied_delivery);
    let mut has_unmasked = false;
    super::finish_management_on_line(&line, true, |_, _| {
        has_unmasked = true;
    });
    drop(removed);
    assert!(!has_unmasked);
    let mut entry = line.lock();
    assert!(!entry.is_teardown_in_progress());
    assert!(entry.is_enabled_for_tests());
    assert_eq!(entry.disable_depth_for_tests(), 0);
    assert_eq!(entry.action_count(), 1);
    assert_eq!(entry.configured_generation_for_tests(), configured_before);
    assert!(!entry.can_run_actions());
    assert!(!entry.needs_replay());
    assert!(entry.is_pending);
    assert_eq!(Arc::strong_count(&owner), 2);
    assert_eq!(Arc::strong_count(&offered), 1);
    let mut observed = Vec::with_capacity(2);
    let recovered = super::register_shared_locked(virq, &mut entry, &offered, |plan| {
        observed.push((plan.configure, plan.enable, Arc::strong_count(&offered)));
        Ok(())
    })
    .unwrap()
    .unwrap();
    assert_eq!(
        observed.as_slice(),
        &[(true, None, 2), (false, Some(true), 2)]
    );
    assert!(entry.can_run_actions());
    assert!(entry.needs_replay());
    assert!(!entry.needs_configuration_restore());
    assert_eq!(
        entry.configured_generation_for_tests(),
        Some(entry.generation_for_tests())
    );
    assert_eq!(entry.action_count(), 2);
    drop(entry);
    assert!(super::take_from_line(&line, Some(survivor)).is_some());
    assert!(super::take_from_line(&line, Some(recovered)).is_some());
    assert_eq!(Arc::strong_count(&owner), 1);
    assert_eq!(Arc::strong_count(&offered), 1);
}

#[def_test]
fn regular_retry_restores_previously_enabled_delivery_only_for_auto_enable() {
    let virq = 0x3174;
    let handler: crate::Handler = Arc::new(test_handler);
    for is_auto_enable in [false, true] {
        let mut entry = IrqStateDesc::test_new(IrqDesc::from_virq(virq));
        let enabled = entry.prepare_legacy_enable();
        entry.commit_platform_plan(enabled);
        entry.update_desc(entry.desc.with_flags(IrqFlags::WAKEUP_SOURCE));
        let options = if is_auto_enable {
            super::IrqRegisterOptions::auto_enable()
        } else {
            super::IrqRegisterOptions::disabled()
        };
        let failed = super::register_locked(virq, &mut entry, &handler, options, |plan| {
            assert!(plan.configure);
            Err(IrqDescError::ConfigurationFailed { hwirq: virq })
        });
        assert_eq!(
            failed,
            Err(IrqDescError::ConfigurationFailed { hwirq: virq })
        );
        assert!(entry.needs_configuration_restore());
        assert_eq!(entry.action_count(), 0);
        let mut observed = Vec::with_capacity(2);
        assert!(
            super::register_locked(virq, &mut entry, &handler, options, |plan| {
                observed.push((plan.configure, plan.enable, Arc::strong_count(&handler)));
                Ok(())
            })
            .unwrap()
        );
        assert_eq!(
            observed.as_slice(),
            &[(true, None, 2), (false, Some(is_auto_enable), 2)]
        );
        assert_eq!(entry.can_run_actions(), is_auto_enable);
        assert_eq!(
            entry.disable_depth_for_tests(),
            usize::from(!is_auto_enable)
        );
        assert_eq!(
            entry.configured_generation_for_tests(),
            Some(entry.generation_for_tests())
        );
        assert!(!entry.prepare_reconfigure_if_stale().configure);
        assert!(!entry.needs_configuration_restore());
    }
    assert_eq!(Arc::strong_count(&handler), 1);
}

#[def_test]
fn disabled_recovery_clears_logical_enable_before_later_enable() {
    let virq = 0x3175;
    let handler: crate::Handler = Arc::new(test_handler);
    let mut entry = IrqStateDesc::test_new(IrqDesc::from_virq(virq));
    let enabled = entry.prepare_legacy_enable();
    entry.commit_platform_plan(enabled);
    entry.mark_configuration_failed();
    assert!(entry.is_enabled_for_tests());
    assert_eq!(entry.action_count(), 0);
    let mut observed = Vec::with_capacity(2);
    assert!(
        super::register_locked(
            virq,
            &mut entry,
            &handler,
            super::IrqRegisterOptions::disabled(),
            |plan| {
                observed.push((plan.configure, plan.enable));
                Ok(())
            },
        )
        .unwrap()
    );
    assert_eq!(observed.as_slice(), &[(true, None), (false, Some(false))]);
    assert_eq!(entry.disable_depth_for_tests(), 1);
    assert!(!entry.is_enabled_for_tests());
    assert!(!entry.prepare_reconfigure_if_stale().configure);
    assert_eq!(
        entry.configured_generation_for_tests(),
        Some(entry.generation_for_tests())
    );
    let enable = entry.prepare_enable_irq();
    assert_eq!(enable.enable, Some(true));
    assert!(!enable.configure);
}

#[def_test]
fn regular_registration_keeps_line_locked_through_backend_update() -> unittest::TestResult {
    let virq = 0x315e;
    let handler: crate::Handler = Arc::new(test_handler);
    for auto_enable in [false, true] {
        let line = Arc::new(IrqLine::new(IrqStateDesc::test_new(IrqDesc::from_virq(
            virq,
        ))));
        let has_atomic_exclusion = {
            let _guard = line.lock();
            line.is_state_locked_for_tests()
        };
        if !has_atomic_exclusion {
            return unittest::TestResult::Ignored;
        }
        let options = if auto_enable {
            super::IrqRegisterOptions::auto_enable()
        } else {
            super::IrqRegisterOptions::disabled()
        };
        let mut observed = Vec::with_capacity(2);
        let installed = super::register_locked(virq, &mut line.lock(), &handler, options, |plan| {
            observed.push((
                karch::local_irq_enabled(),
                line.is_state_locked_for_tests(),
                plan.desc.logical_irq(),
                plan.configure,
                plan.enable,
                Arc::strong_count(&handler),
            ));
            Ok(())
        });
        assert_eq!(installed, Ok(true));
        if auto_enable {
            assert_eq!(
                observed.as_slice(),
                &[
                    (false, true, Some(virq), true, None, 2),
                    (false, true, Some(virq), false, Some(true), 2),
                ]
            );
        } else {
            assert_eq!(
                observed.as_slice(),
                &[
                    (false, true, Some(virq), true, None, 2),
                    (false, true, Some(virq), false, None, 2),
                ]
            );
        }
        assert!(!line.is_state_locked_for_tests());
        assert_eq!(line.lock().action_count(), 1);
        assert_eq!(line.lock().is_enabled_for_tests(), auto_enable);
        assert_eq!(
            line.lock().disable_depth_for_tests(),
            usize::from(!auto_enable)
        );
    }
    assert_eq!(Arc::strong_count(&handler), 1);
    unittest::TestResult::Ok
}

#[def_test]
fn shared_registration_keeps_line_locked_without_reenabling_later_actions() -> unittest::TestResult
{
    let virq = 0x315f;
    let line = Arc::new(IrqLine::new(IrqStateDesc::test_new(IrqDesc::from_virq(
        virq,
    ))));
    let has_atomic_exclusion = {
        let _guard = line.lock();
        line.is_state_locked_for_tests()
    };
    if !has_atomic_exclusion {
        return unittest::TestResult::Ignored;
    }
    let handler: crate::Handler = Arc::new(test_handler);
    let mut tokens = Vec::new();
    for is_first in [true, false] {
        let mut observed = Vec::with_capacity(2);
        let token = super::register_shared_locked(virq, &mut line.lock(), &handler, |plan| {
            observed.push((
                karch::local_irq_enabled(),
                line.is_state_locked_for_tests(),
                plan.configure,
                plan.enable,
                Arc::strong_count(&handler),
            ));
            Ok(())
        })
        .unwrap()
        .unwrap();
        if is_first {
            assert_eq!(
                observed.as_slice(),
                &[
                    (false, true, true, None, 2),
                    (false, true, false, Some(true), 2),
                ]
            );
        } else {
            assert_eq!(observed.as_slice(), &[(false, true, false, None, 3)]);
        }
        tokens.push(token);
        assert!(!line.is_state_locked_for_tests());
        // A later shared registration must not undo the owner's disable.
        if is_first {
            let mut entry = line.lock();
            let plan = entry.prepare_disable_irq_nosync();
            entry.commit_platform_plan(plan);
        }
    }
    assert_ne!(tokens[0], tokens[1]);
    assert_eq!(line.lock().action_count(), 2);
    assert_eq!(line.lock().disable_depth_for_tests(), 1);
    assert!(!line.lock().is_enabled_for_tests());
    unittest::TestResult::Ok
}

#[def_test]
fn registration_preflight_and_install_share_resolved_line_state() {
    let virq = 0x3165;
    let desc = IrqDesc::from_virq(virq);
    let _binding = SyntheticLine::new(desc);
    try_map(desc).unwrap();
    let line = IRQ_STATE.lock().descs.get(&virq).unwrap().clone();
    let has_atomic_exclusion = {
        let _guard = line.lock();
        line.is_state_locked_for_tests()
    };
    let richer = desc.with_flags(IrqFlags::PER_CPU);
    let were_irqs_enabled = karch::local_irq_enabled();
    let rejected = super::with_registration_line_checked(
        richer.into(),
        super::ManagementMode::Task,
        |entry| {
            assert_eq!(entry.desc, desc);
            Ok(false)
        },
        |_, _, _| -> Result<(), IrqDescError> {
            panic!("rejected preflight must not run installation")
        },
    )
    .unwrap();
    assert!(rejected.is_none());
    assert_eq!(line.lock().desc, desc);
    assert_eq!(karch::local_irq_enabled(), were_irqs_enabled);

    let handler: crate::Handler = Arc::new(test_handler);
    let accepted = super::with_registration_line_checked(
        richer.into(),
        super::ManagementMode::Task,
        |entry| {
            assert_eq!(entry.desc, desc);
            assert!(!has_atomic_exclusion || line.is_state_locked_for_tests());
            Ok(true)
        },
        |resolved_virq, _, entry| {
            assert_eq!(resolved_virq, virq);
            assert_eq!(entry.desc, richer);
            assert!(!has_atomic_exclusion || line.is_state_locked_for_tests());
            super::register_locked(
                virq,
                entry,
                &handler,
                super::IrqRegisterOptions::disabled(),
                |_| Ok(()),
            )
        },
    )
    .unwrap();
    assert_eq!(accepted, Some(true));
    assert_eq!(karch::local_irq_enabled(), were_irqs_enabled);
    assert_eq!(line.lock().action_count(), 1);
    assert_eq!(line.lock().desc, richer);
    drop(try_free_irq(virq).unwrap().unwrap());
    assert!(
        IRQ_STATE
            .lock()
            .descs
            .get(&virq)
            .is_some_and(|stored| Arc::ptr_eq(&stored, &line))
    );
    assert!(IRQ_STATE.lock().remove_if_unused(virq).is_some());
    assert_eq!(Arc::strong_count(&handler), 1);
}

#[def_test]
fn registration_rejection_preserves_handler_ownership_and_skips_backend() {
    let virq = 0x3160;
    let line = Arc::new(IrqLine::new(IrqStateDesc::test_new(IrqDesc::from_virq(
        virq,
    ))));
    let owner: crate::Handler = Arc::new(test_handler);
    assert!(
        line.lock()
            .install_regular_action(IrqAction::regular(owner.clone()))
    );
    let rejected: crate::Handler = Arc::new(test_handler);
    let mut has_called_backend = false;
    assert!(
        !super::register_locked(
            virq,
            &mut line.lock(),
            &rejected,
            super::IrqRegisterOptions::auto_enable(),
            |_| {
                has_called_backend = true;
                Ok(())
            },
        )
        .unwrap()
    );
    assert!(
        super::register_shared_locked(virq, &mut line.lock(), &rejected, |_| {
            has_called_backend = true;
            Ok(())
        },)
        .unwrap()
        .is_none()
    );
    assert!(!has_called_backend);
    assert_eq!(Arc::strong_count(&rejected), 1);
    assert_eq!(Arc::strong_count(&owner), 2);
    assert_eq!(line.lock().action_count(), 1);
    assert!(!line.lock().is_enabled_for_tests());
}

#[def_test]
fn shared_reconfiguration_precedes_new_owner_without_restoring_disabled_delivery() {
    let virq = 0x316e;
    let mut entry = IrqStateDesc::test_new(IrqDesc::from_virq(virq));
    let first: crate::Handler = Arc::new(test_handler);
    let next: crate::Handler = Arc::new(test_handler);
    let first_token = super::register_shared_locked(virq, &mut entry, &first, |_| Ok(()))
        .unwrap()
        .unwrap();
    let disable = entry.prepare_disable_irq_nosync();
    entry.commit_platform_plan(disable);
    entry.update_desc(entry.desc.with_flags(IrqFlags::WAKEUP_SOURCE));
    let generation = entry.generation_for_tests();
    let mut observed = Vec::with_capacity(2);
    let next_token = super::register_shared_locked(virq, &mut entry, &next, |plan| {
        observed.push((
            plan.configure,
            plan.enable,
            plan.flow_unmask.is_some(),
            Arc::strong_count(&first),
            Arc::strong_count(&next),
        ));
        Ok(())
    })
    .unwrap()
    .unwrap();
    assert_eq!(
        observed.as_slice(),
        &[(true, None, false, 2, 2), (false, None, false, 2, 2),]
    );
    assert_ne!(first_token, next_token);
    assert_eq!(entry.configured_generation_for_tests(), Some(generation));
    assert_eq!(entry.action_count(), 2);
    assert_eq!(entry.disable_depth_for_tests(), 1);
    assert!(!entry.is_enabled_for_tests());
    drop(super::take_from_state(&mut entry, Some(first_token)));
    drop(super::take_from_state(&mut entry, Some(next_token)));
    assert_eq!(Arc::strong_count(&first), 1);
    assert_eq!(Arc::strong_count(&next), 1);
}

#[def_test]
fn withdrawal_keeps_line_locked_through_mask_and_returns_the_owner() -> unittest::TestResult {
    let virq = 0x3161;
    let handler: crate::Handler = Arc::new(test_handler);
    let mut entry = IrqStateDesc::test_new(IrqDesc::from_virq(virq));
    assert!(entry.install_regular_action(IrqAction::regular(handler.clone())));
    let plan = entry.prepare_auto_enable();
    entry.commit_platform_plan(plan);
    // Accounting-only credit: no snapshot borrows the action in this fixture.
    entry.begin_running_dispatch();
    let line = Arc::new(IrqLine::new(entry));
    let has_atomic_exclusion = {
        let _guard = line.lock();
        line.is_state_locked_for_tests()
    };
    if !has_atomic_exclusion {
        return unittest::TestResult::Ignored;
    }
    let mut observed = None;
    let removed = super::withdraw_on_line(virq, &line, None, |plan| {
        observed = Some((
            karch::local_irq_enabled(),
            line.is_state_locked_for_tests(),
            plan.enable,
            plan.configure,
            Arc::strong_count(&handler),
        ));
    })
    .unwrap();
    assert_eq!(observed, Some((false, true, Some(false), false, 2)));
    assert!(!line.is_state_locked_for_tests());
    assert_eq!(line.lock().action_count(), 0);
    assert_eq!(line.lock().teardown_depth_for_tests(), 1);
    assert!(line.lock().is_running);
    assert!(!line.lock().is_enabled_for_tests());
    assert_eq!(Arc::strong_count(&handler), 2);
    drop(removed);
    assert_eq!(Arc::strong_count(&handler), 1);
    let wake = line.lock().finish_running_dispatch();
    drop(wake);
    line.lock().finish_teardown();
    unittest::TestResult::Ok
}

#[def_test]
fn withdrawn_line_rejects_enable_while_disable_keeps_original_drain() {
    let virq = 0x3164;
    let registry = kspin::SpinNoIrq::new(super::IrqState::new());
    let handler: crate::Handler = Arc::new(test_handler);
    let mut entry = IrqStateDesc::test_new(IrqDesc::from_virq(virq));
    assert!(entry.install_regular_action(IrqAction::regular(handler.clone())));
    let plan = entry.prepare_auto_enable();
    entry.commit_platform_plan(plan);
    entry.begin_management();
    // Accounting-only credit: immediate helper reclaim has no reader to drain.
    entry.begin_running_dispatch();
    let line = Arc::new(IrqLine::new(entry));
    registry.lock().descs.insert(virq, line.clone());
    let mut has_masked = false;
    let removed = super::withdraw_on_line(virq, &line, None, |plan| {
        assert_eq!(plan.enable, Some(false));
        has_masked = true;
    })
    .unwrap();
    assert!(has_masked);
    for force in [false, true] {
        let mut has_enabled = false;
        let result = super::enable_line(&registry, virq.into(), force, |_| {
            has_enabled = true;
            Ok(())
        });
        assert!(matches!(result, Err(IrqDescError::TeardownInProgress { virq: id }) if id == virq));
        assert!(!has_enabled);
    }
    let drain = super::disable_line(&registry, virq.into(), false, |plan| {
        assert_eq!(plan.enable, None);
    })
    .unwrap();
    assert!(Arc::ptr_eq(&drain.line, &line));
    assert_eq!(line.lock().disable_depth_for_tests(), 2);
    assert!(line.lock().is_running);
    let wake = line.lock().finish_running_dispatch();
    drop(wake);
    line.lock().finish_management(true);
    assert!(
        registry
            .lock()
            .descs
            .get(&virq)
            .is_some_and(|stored| Arc::ptr_eq(&stored, &line))
    );
    assert_eq!(Arc::strong_count(&handler), 2);
    drop(removed);
    assert_eq!(Arc::strong_count(&handler), 1);
}

#[def_test]
fn missing_action_withdrawal_preserves_empty_line_control_state() {
    let virq = 0x3165;
    for is_enabled in [false, true] {
        for token in [None, Some(crate::IrqActionToken::new(1))] {
            let mut entry = IrqStateDesc::test_new(IrqDesc::from_virq(virq));
            if is_enabled {
                // Legacy startup can enable a Line before installing an action.
                let plan = entry.prepare_legacy_enable();
                entry.commit_platform_plan(plan);
            }
            let snapshot = |entry: &IrqStateDesc| {
                (
                    entry.is_enabled_for_tests(),
                    entry.disable_depth_for_tests(),
                    entry.teardown_depth_for_tests(),
                    entry.action_count(),
                    entry.generation_for_tests(),
                    entry.configured_generation_for_tests(),
                )
            };
            let before = snapshot(&entry);
            let line = Arc::new(IrqLine::new(entry));
            let mut has_applied_plan = false;
            let removed = super::withdraw_on_line(virq, &line, token, |_| {
                has_applied_plan = true;
            });
            assert!(removed.is_none());
            assert!(!has_applied_plan);
            assert_eq!(snapshot(&line.lock()), before);
        }
    }
}

#[def_test]
fn shared_withdrawal_masks_only_the_last_owned_action() {
    let virq = 0x3162;
    let handler: crate::Handler = Arc::new(test_handler);
    let mut entry = IrqStateDesc::test_new(IrqDesc::from_virq(virq));
    let first = entry.install_shared_action(handler.clone()).unwrap();
    let last = entry.install_shared_action(handler.clone()).unwrap();
    let plan = entry.prepare_auto_enable();
    entry.commit_platform_plan(plan);
    let line = Arc::new(IrqLine::new(entry));
    let mut has_applied_plan = false;
    let removed = super::withdraw_on_line(virq, &line, Some(first), |_| {
        has_applied_plan = true;
    });
    assert!(removed.is_some());
    assert!(!has_applied_plan);
    assert_eq!(line.lock().action_count(), 1);
    assert!(line.lock().is_enabled_for_tests());
    assert!(!line.lock().is_teardown_in_progress());
    drop(removed);

    let rejected = super::withdraw_on_line(virq, &line, None, |_| {
        has_applied_plan = true;
    });
    assert!(rejected.is_none());
    assert!(!has_applied_plan);
    assert_eq!(line.lock().action_count(), 1);
    assert_eq!(line.lock().teardown_depth_for_tests(), 0);
    let mut observed_enable = None;
    let removed = super::withdraw_on_line(virq, &line, Some(last), |plan| {
        observed_enable = Some(plan.enable);
        has_applied_plan = true;
    });
    assert_eq!(observed_enable, Some(Some(false)));
    assert!(removed.is_some());
    assert!(has_applied_plan);
    assert_eq!(line.lock().action_count(), 0);
    assert!(!line.lock().is_enabled_for_tests());
    line.lock().finish_teardown();
    drop(removed);
    assert_eq!(Arc::strong_count(&handler), 1);
}

#[def_test(serial)]
fn shared_survivor_dispatches_before_removed_action_reclamation() {
    static LEVEL: crate::LevelChip = crate::LevelChip {
        mask_ack: |_| {},
        unmask: |_| {},
    };
    static EDGE: crate::EdgeChip = crate::EdgeChip {
        mask: |_| {},
        unmask: |_| {},
        retrigger: |_| false,
    };
    static CHIP: crate::FasteoiChip = crate::FasteoiChip {
        mask: |_| {},
        unmask: |_| {},
        resend: crate::FasteoiResend::Software,
    };
    struct RestoreLine(usize);
    impl Drop for RestoreLine {
        fn drop(&mut self) {
            let removed = IRQ_STATE.lock().descs.remove(&self.0);
            drop(removed);
        }
    }
    for flow in [
        crate::IrqFlow::Level(&LEVEL),
        crate::IrqFlow::Edge(&EDGE),
        crate::IrqFlow::Fasteoi(&CHIP),
    ] {
        let virq = 0x3174;
        let removed_calls = Arc::new(AtomicUsize::new(0));
        let survivor_calls = Arc::new(AtomicUsize::new(0));
        let observed = removed_calls.clone();
        let removed_owner: crate::Handler = Arc::new(move |_| {
            observed.fetch_add(1, Ordering::Relaxed);
            IrqEvent::HANDLED
        });
        let observed = survivor_calls.clone();
        let survivor_owner: crate::Handler = Arc::new(move |_| {
            observed.fetch_add(1, Ordering::Relaxed);
            IrqEvent::HANDLED
        });
        let mut entry = IrqStateDesc::test_new(IrqDesc::from_virq(virq));
        entry.set_flow_for_tests(flow);
        let removed_token = entry.install_shared_action(removed_owner.clone()).unwrap();
        let survivor_token = entry.install_shared_action(survivor_owner.clone()).unwrap();
        let plan = entry.prepare_auto_enable();
        entry.commit_platform_plan(plan);
        entry.begin_management();
        // Model an older serialized claim through withdrawal. It must finish
        // before a new ordinary handler can run, but not before unlink itself.
        entry.begin_running_dispatch();
        let line = Arc::new(IrqLine::new(entry));
        assert!(IRQ_STATE.lock().descs.insert(virq, line.clone()).is_none());
        let _restore = RestoreLine(virq);
        let retired =
            super::withdraw_action_on_line(virq, &line, Some(removed_token), false, |_| {
                panic!("shared survivor must not be masked by removal")
            })
            .unwrap()
            .unwrap();
        assert!(!line.lock().is_teardown_in_progress());
        assert!(line.lock().can_run_actions());
        assert_eq!(Arc::strong_count(&removed_owner), 2);
        let wake = line.lock().finish_running_dispatch();
        drop(wake);
        let mut completions = 0;
        assert_eq!(
            crate::runtime::dispatch::dispatch_claim(IrqRef::Virq(virq), || {
                completions += 1;
            }),
            Some(virq)
        );
        assert_eq!(completions, 1);
        assert_eq!(removed_calls.load(Ordering::Relaxed), 0);
        assert_eq!(survivor_calls.load(Ordering::Relaxed), 1);
        assert!(!line.lock().is_running);
        super::IrqDrain::new(virq, &line).wait();
        // SAFETY: the synchronous survivor traversal and older running claim
        // have ended. The original Line drained under its lock while
        // fixture management excludes mutation; no snapshot can reach the
        // detached node. No short lock is held during reclaim.
        let removed = unsafe { retired.reclaim() };
        // A survivor may already be executing when removal finishes. Management
        // completion must not unmask behind the flow owner's back.
        line.lock().is_running = true;
        super::finish_management_on_line(&line, true, |_, _| {
            panic!("shared removal must not restore another flow owner's mask");
        });
        line.lock().is_running = false;
        drop(removed);
        assert_eq!(Arc::strong_count(&removed_owner), 1);
        assert!(super::take_from_line(&line, Some(survivor_token)).is_some());
        assert_eq!(Arc::strong_count(&survivor_owner), 1);
    }
}

#[def_test]
fn enable_final_admission_rejects_teardown_after_earlier_observation() {
    let virq = 0x3163;
    for force in [false, true] {
        let mut entry = IrqStateDesc::test_new(IrqDesc::from_virq(virq));
        assert!(entry.install_regular_action(IrqAction::regular(Arc::new(test_handler))));
        let plan = entry.prepare_register_disabled();
        entry.commit_platform_plan(plan);
        let line = Arc::new(IrqLine::new(entry));
        assert!(!line.lock().is_teardown_in_progress());
        // Model withdrawal after an earlier observation, without depending on
        // another CPU's timing. Final admission must inspect the current state.
        line.lock().begin_teardown();
        let mut entry = line.lock();
        let desc = entry.desc;
        let result = super::prepare_enable_plan(&entry, virq, force);
        assert!(matches!(result, Err(IrqDescError::TeardownInProgress { virq: id }) if id == virq));
        assert_eq!(entry.desc, desc);
        assert_eq!(entry.disable_depth_for_tests(), 1);
        assert_eq!(entry.action_count(), 1);
        assert!(!entry.is_enabled_for_tests());
        entry.finish_teardown();
        let plan = super::prepare_enable_plan(&entry, virq, force).unwrap();
        assert_eq!(plan.enable, Some(true));
        assert_eq!(entry.disable_depth_for_tests(), 1);
        entry.commit_platform_plan(plan);
        assert_eq!(entry.disable_depth_for_tests(), 0);
    }
}
// UP spinlocks have no contended state to inspect; the lock-exclusion probe
// requires SMP even though nested-enable semantics apply to both builds.
#[def_test(serial)]
fn enable_backend_holds_original_line_through_nested_and_forced_updates() -> unittest::TestResult {
    let registry = kspin::SpinNoIrq::new(super::IrqState::new());
    let has_atomic_exclusion = {
        let _guard = registry.lock();
        registry.is_locked()
    };
    if !has_atomic_exclusion {
        return unittest::TestResult::Ignored;
    }
    let virq = 0x315b;
    let mut entry = IrqStateDesc::test_new(IrqDesc::from_virq(virq));
    assert!(entry.install_regular_action(IrqAction::regular(Arc::new(test_handler))));
    let plan = entry.prepare_disable_irq_nosync();
    entry.commit_platform_plan(plan);
    let plan = entry.prepare_disable_irq_nosync();
    entry.commit_platform_plan(plan);
    let line = Arc::new(IrqLine::new(entry));
    registry.lock().descs.insert(virq, line.clone());

    for (force, expected_enable, configure, depth) in [
        (false, None, false, 1),
        (false, Some(true), true, 0),
        (false, None, false, 0),
        (true, Some(true), false, 0),
    ] {
        let irq_before = karch::local_irq_enabled();
        let mut observed = None;
        super::enable_line(&registry, virq.into(), force, |plan| {
            // This registry is private; remote IRQs cannot affect the probe.
            observed = Some((
                karch::local_irq_enabled(),
                registry.try_lock().is_some(),
                line.is_state_locked_for_tests(),
                plan.desc.logical_irq(),
                plan.enable,
                plan.configure,
            ));
            Ok(())
        })
        .unwrap();
        assert_eq!(karch::local_irq_enabled(), irq_before);
        assert_eq!(
            observed,
            Some((false, true, true, Some(virq), expected_enable, configure))
        );
        assert!(!line.is_state_locked_for_tests());
        assert_eq!(line.lock().disable_depth_for_tests(), depth);
        assert_eq!(line.lock().is_enabled_for_tests(), depth == 0);
        assert!(Arc::ptr_eq(
            &registry.lock().descs.get(&virq).unwrap(),
            &line
        ));
    }
    {
        let _guard = kspin::NoPreemptIrqSave::new();
        for force in [false, true] {
            super::enable_line(&registry, virq.into(), force, |_| Ok(())).unwrap();
            assert!(!karch::local_irq_enabled());
        }
    }
    unittest::TestResult::Ok
}

#[def_test(serial)]
fn management_completion_preserves_irq_state_and_restores_under_line_lock() -> unittest::TestResult
{
    use unittest::{assert, assert_eq};

    static CHIP: crate::FasteoiChip = crate::FasteoiChip {
        mask: |_| {},
        unmask: |_| {},
        resend: crate::FasteoiResend::Software,
    };
    let registry = kspin::SpinNoIrq::new(super::IrqState::new());
    let has_atomic_exclusion = {
        let _guard = registry.lock();
        registry.is_locked()
    };
    if !has_atomic_exclusion {
        return unittest::TestResult::Ignored;
    }
    for (index, (has_action, is_enabled, has_removed)) in [
        (true, true, true),
        (true, false, true),
        (false, true, true),
        (true, true, false),
    ]
    .into_iter()
    .enumerate()
    {
        let virq = 0x3160 + index;
        let mut entry = IrqStateDesc::test_new(IrqDesc::from_virq(virq));
        entry.set_flow_for_tests(crate::IrqFlow::Fasteoi(&CHIP));
        if has_action {
            assert!(entry.install_regular_action(IrqAction::regular(Arc::new(test_handler))));
        }
        if is_enabled {
            let plan = entry.prepare_legacy_enable();
            entry.commit_platform_plan(plan);
        }
        entry.begin_management();
        if has_removed {
            entry.begin_teardown();
        }
        let line = Arc::new(IrqLine::new(entry));
        registry.lock().descs.insert(virq, line.clone());
        let irq_before = karch::local_irq_enabled();
        let mut observed = None;
        super::finish_management_on_line(&line, has_removed, |_, desc| {
            // A private registry makes failure specific to this path,
            // not unrelated remote IRQ activity. Never block on the Line.
            observed = Some((
                registry.try_lock().is_some(),
                line.is_state_locked_for_tests(),
                !karch::local_irq_enabled(),
                desc.logical_irq(),
            ));
        });
        assert_eq!(karch::local_irq_enabled(), irq_before);
        let expected =
            (has_action && is_enabled && has_removed).then_some((true, true, true, Some(virq)));
        assert_eq!(observed, expected);
        assert!(!line.lock().is_teardown_in_progress());
        assert!(
            registry
                .lock()
                .descs
                .get(&virq)
                .is_some_and(|stored| Arc::ptr_eq(&stored, &line))
        );
    }
    unittest::TestResult::Ok
}

#[def_test(serial)]
fn enable_rejections_do_not_call_backend_or_change_line() {
    let irq_before = karch::local_irq_enabled();
    let registry = kspin::SpinNoIrq::new(super::IrqState::new());
    let virq = 0x315c;
    let mut has_called_backend = false;
    let result = super::enable_line(&registry, virq.into(), false, |_| {
        has_called_backend = true;
        Ok(())
    });
    assert!(matches!(result, Err(IrqDescError::UnknownIrq)));
    assert_eq!(karch::local_irq_enabled(), irq_before);
    assert!(!has_called_backend);
    assert!(registry.lock().descs.is_empty());

    let mut entry = IrqStateDesc::test_new(IrqDesc::from_virq(virq));
    let plan = entry.prepare_disable_irq_nosync();
    entry.commit_platform_plan(plan);
    let line = Arc::new(IrqLine::new(entry));
    registry.lock().descs.insert(virq, line.clone());
    let result = super::enable_line(&registry, virq.into(), false, |_| {
        has_called_backend = true;
        Ok(())
    });
    assert!(matches!(result, Err(IrqDescError::NoIrqAction { virq: id }) if id == virq));
    assert_eq!(karch::local_irq_enabled(), irq_before);
    assert!(!has_called_backend);
    assert_eq!(line.lock().disable_depth_for_tests(), 1);
    assert!(!line.lock().is_enabled_for_tests());
    line.lock().begin_teardown();
    for force in [false, true] {
        let result = super::enable_line(&registry, virq.into(), force, |_| {
            has_called_backend = true;
            Ok(())
        });
        assert!(matches!(result, Err(IrqDescError::TeardownInProgress { virq: id }) if id == virq));
        assert_eq!(karch::local_irq_enabled(), irq_before);
        assert!(!has_called_backend);
        assert_eq!(line.lock().disable_depth_for_tests(), 1);
        assert!(!line.lock().is_enabled_for_tests());
    }
    line.lock().finish_teardown();
}

#[def_test]
fn controller_update_excludes_only_its_original_line() -> unittest::TestResult {
    let registry = kspin::SpinNoIrq::new(super::IrqState::new());
    let has_atomic_exclusion = {
        let _guard = registry.lock();
        registry.is_locked()
    };
    if !has_atomic_exclusion {
        return unittest::TestResult::Ignored;
    }
    let first_virq = 0x316a;
    let second_virq = 0x316b;
    let make_line = |virq| {
        let mut entry = IrqStateDesc::test_new(IrqDesc::from_virq(virq));
        assert!(entry.install_regular_action(IrqAction::regular(Arc::new(test_handler))));
        let plan = entry.prepare_register_disabled();
        entry.commit_platform_plan(plan);
        Arc::new(IrqLine::new(entry))
    };
    let first = make_line(first_virq);
    let second = make_line(second_virq);
    {
        let mut state = registry.lock();
        state.descs.insert(first_virq, first.clone());
        state.descs.insert(second_virq, second.clone());
    }

    for (virq, original, other) in [
        (first_virq, &first, &second),
        (second_virq, &second, &first),
    ] {
        let were_irqs_enabled = karch::local_irq_enabled();
        let observe = |plan: super::IrqPlatformPlan| {
            assert_eq!(plan.desc.logical_irq(), Some(virq));
            assert!(!karch::local_irq_enabled());
            assert!(original.is_state_locked_for_tests());
            // Probe the directory-to-other-Line lock order without making a
            // controller callback reenter management. Private state excludes
            // unrelated IRQ activity from this lock-availability assertion.
            let state = registry.try_lock().unwrap();
            assert!(Arc::ptr_eq(&state.descs.get(&virq).unwrap(), original));
            assert!(!other.is_state_locked_for_tests());
            let other_entry = other.lock();
            assert_eq!(other_entry.disable_depth_for_tests(), 1);
            assert!(original.is_state_locked_for_tests());
        };
        super::enable_line(&registry, virq.into(), false, |plan| {
            observe(plan);
            Ok(())
        })
        .unwrap();
        super::enable_line(&registry, virq.into(), true, |plan| {
            observe(plan);
            Ok(())
        })
        .unwrap();
        let drain = super::disable_line(&registry, virq.into(), false, observe).unwrap();
        assert!(Arc::ptr_eq(&drain.line, original));
        assert_eq!(karch::local_irq_enabled(), were_irqs_enabled);
        assert!(!original.is_state_locked_for_tests());
        assert_eq!(original.lock().disable_depth_for_tests(), 1);
    }
    unittest::TestResult::Ok
}

#[def_test(serial)]
fn legacy_enable_uses_prepared_line_for_backend_update() {
    let registry = kspin::SpinNoIrq::new(super::IrqState::new());
    let virq = 0x315d;
    let desc = IrqDesc::from_virq(virq);
    let prepared = Arc::new(IrqLine::new(IrqStateDesc::test_new(desc)));
    registry
        .lock()
        .try_resolve_desc(desc, Some(&prepared))
        .unwrap();
    let irq_before = karch::local_irq_enabled();
    let mut retained = None;
    let mut observed = None;
    super::enable_line(&registry, virq.into(), true, |plan| {
        observed = Some((plan.configure, plan.enable, plan.desc.logical_irq()));
        retained = Some(registry.lock().descs.get(&virq).unwrap().clone());
        Ok(())
    })
    .unwrap();
    assert_eq!(karch::local_irq_enabled(), irq_before);
    assert_eq!(observed, Some((true, Some(true), Some(virq))));
    let line = retained.unwrap();
    assert!(Arc::ptr_eq(&line, &prepared));
    assert!(Arc::ptr_eq(
        &registry.lock().descs.get(&virq).unwrap(),
        &line
    ));
    assert_eq!(line.lock().disable_depth_for_tests(), 0);
    assert!(line.lock().is_enabled_for_tests());
}

// UP spinlocks have no contended state to inspect; the lock-exclusion probe
// requires SMP even though nested-disable semantics apply to both builds.
#[def_test(serial)]
fn disable_backend_holds_original_line_without_registry_lock() -> unittest::TestResult {
    // Real IRQs on other CPUs may take IRQ_STATE even during serial tests.
    // A private registry makes try_lock failure specific to this transaction.
    let registry = kspin::SpinNoIrq::new(super::IrqState::new());
    let has_atomic_exclusion = {
        let _guard = registry.lock();
        registry.is_locked()
    };
    if !has_atomic_exclusion {
        return unittest::TestResult::Ignored;
    }
    let virq = 0x3159;
    let mut entry = IrqStateDesc::test_new(IrqDesc::from_virq(virq));
    assert!(entry.install_regular_action(IrqAction::regular(Arc::new(test_handler))));
    let plan = entry.prepare_enable_irq();
    assert_eq!(plan.enable, Some(true));
    entry.commit_platform_plan(plan);
    let line = Arc::new(IrqLine::new(entry));
    registry.lock().descs.insert(virq, line.clone());

    for expected_enable in [Some(false), None] {
        let irq_before = karch::local_irq_enabled();
        let mut observed = None;
        let drain = super::disable_line(&registry, virq.into(), false, |plan| {
            observed = Some((
                karch::local_irq_enabled(),
                registry.try_lock().is_some(),
                line.is_state_locked_for_tests(),
                plan.desc.logical_irq(),
                plan.enable,
                plan.configure,
            ));
        })
        .unwrap();
        assert_eq!(karch::local_irq_enabled(), irq_before);
        assert_eq!(
            observed,
            Some((false, true, true, Some(virq), expected_enable, false))
        );
        assert!(Arc::ptr_eq(&drain.line, &line));
        assert!(!line.is_state_locked_for_tests());
    }
    assert_eq!(line.lock().disable_depth_for_tests(), 2);
    assert!(!line.lock().is_enabled_for_tests());
    {
        let _guard = kspin::NoPreemptIrqSave::new();
        let drain = super::disable_line(&registry, virq.into(), false, |_| {}).unwrap();
        assert!(!karch::local_irq_enabled());
        assert!(Arc::ptr_eq(&drain.line, &line));
    }
    unittest::TestResult::Ok
}

#[def_test(serial)]
fn disable_missing_line_does_not_call_backend() {
    let irq_before = karch::local_irq_enabled();
    let registry = kspin::SpinNoIrq::new(super::IrqState::new());
    let virq = 0x315a;
    assert!(!registry.lock().descs.contains_key(&virq));
    let mut has_called_backend = false;
    let result = super::disable_line(&registry, virq.into(), false, |_| has_called_backend = true);
    assert_eq!(karch::local_irq_enabled(), irq_before);
    assert!(matches!(result, Err(IrqDescError::UnknownIrq)));
    assert!(!has_called_backend);
    assert!(!registry.lock().descs.contains_key(&virq));
}

#[def_test(serial)]
fn test_dispatch_tracks_line_running_state() {
    IRQ_IN_FLIGHT_OBSERVED.store(0, Ordering::Relaxed);
    let line = Arc::new(IrqLine::new(IrqStateDesc::test_with_runtime(
        IrqDesc::from_virq(IRQ_IN_FLIGHT_TEST_VIRQ),
        Some(IrqAction::regular(Arc::new(test_running_handler))),
    )));
    {
        let mut entry = line.lock();
        let plan = entry.prepare_auto_enable();
        entry.commit_platform_plan(plan);
    }
    {
        let mut state = IRQ_STATE.lock();
        state.descs.insert(IRQ_IN_FLIGHT_TEST_VIRQ, line.clone());
    }

    let pending = PendingIrq::simulated(IrqRef::Virq(IRQ_IN_FLIGHT_TEST_VIRQ));
    super::dispatch_simulated_irq(pending);

    assert_eq!(IRQ_IN_FLIGHT_OBSERVED.load(Ordering::Relaxed), 1);
    {
        let state = IRQ_STATE.lock();
        let retained_line = state
            .descs
            .get(&IRQ_IN_FLIGHT_TEST_VIRQ)
            .expect("descriptor must remain while action exists");
        let entry = retained_line.lock();
        assert!(!entry.is_running);
    }

    assert!(unregister(IRQ_IN_FLIGHT_TEST_VIRQ).is_some());
    assert!(retire_unused_test_line(IRQ_IN_FLIGHT_TEST_VIRQ));
}

fn check_claim_completion_retains_line(has_action: bool) {
    let virq = 0x3158;
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let action = has_action.then(|| {
        IrqAction::regular(Arc::new(move |_| {
            observed.fetch_add(1, Ordering::Relaxed);
            IrqEvent::HANDLED
        }))
    });
    let line = Arc::new(IrqLine::new(IrqStateDesc::test_with_runtime(
        IrqDesc::from_virq(virq),
        action,
    )));
    if has_action {
        let mut entry = line.lock();
        let plan = entry.prepare_auto_enable();
        entry.commit_platform_plan(plan);
    }
    assert!(IRQ_STATE.lock().descs.insert(virq, line.clone()).is_none());
    let completion = line.lock().execution_idle_completion();
    let mut has_completed = false;
    {
        let _irq_guard = kspin::NoPreemptIrqSave::new();
        run_claimed_irq_tail(virq, PendingIrq::simulated(IrqRef::Virq(virq)), |claim| {
            // This is a synthetic claim, not a controller ACK. The injected
            // completion consumes it without sending a fabricated cookie to hardware.
            core::mem::forget(claim);
            assert_eq!(calls.load(Ordering::Relaxed), usize::from(has_action));
            // Ordinary completion holds the Line lock: do not relock it here.
            assert_eq!(completion.is_completed(), !has_action);
            has_completed = true;
        });
    }
    assert!(has_completed);
    assert!(!line.lock().is_running);
    assert!(completion.is_completed());
    // Completion retains both populated and empty descriptors. This fixture
    // never configured hardware; explicitly retire its software ownership.
    let removed = IRQ_STATE.lock().descs.remove(&virq);
    assert!(
        removed
            .as_ref()
            .is_some_and(|stored| Arc::ptr_eq(stored, &line))
    );
    assert!(!IRQ_STATE.lock().descs.contains_key(&virq));
}

#[def_test(serial)]
fn claim_completion_precedes_action_drain() {
    check_claim_completion_retains_line(true);
}

#[def_test(serial)]
fn empty_line_completes_without_execution_admission() {
    check_claim_completion_retains_line(false);
}

#[def_test(serial)]
fn claim_completion_keeps_original_admission_after_numeric_replacement() {
    let virq = 0x315e;
    let original = Arc::new(IrqLine::new(IrqStateDesc::test_new(IrqDesc::from_virq(
        virq,
    ))));
    let replacement = Arc::new(IrqLine::new(IrqStateDesc::test_new(IrqDesc::from_virq(
        virq,
    ))));
    let next = replacement.clone();
    let replaced = Arc::new(AtomicBool::new(false));
    let observed = replaced.clone();
    let weak_original = Arc::downgrade(&original);
    {
        let mut entry = original.lock();
        assert!(
            entry.install_regular_action(IrqAction::regular(Arc::new(move |_| {
                // Bypass production retirement only to test exact-Line retention.
                // The ordinary handler is unlocked; completion will be locked.
                let old = IRQ_STATE.lock().descs.insert(virq, next.clone()).unwrap();
                observed.store(
                    weak_original
                        .upgrade()
                        .is_some_and(|line| Arc::ptr_eq(&line, &old)),
                    Ordering::Release,
                );
                drop(old);
                IrqEvent::HANDLED
            })))
        );
        let plan = entry.prepare_auto_enable();
        entry.commit_platform_plan(plan);
    }
    let original_completion = original.lock().execution_idle_completion();
    let replacement_completion = replacement.lock().execution_idle_completion();
    assert!(
        IRQ_STATE
            .lock()
            .descs
            .insert(virq, original.clone())
            .is_none()
    );
    let mut completions = 0;
    let resolved = {
        let _irq_guard = kspin::NoPreemptIrqSave::new();
        super::dispatch_and_complete(PendingIrq::simulated(IrqRef::Virq(virq)), |claim| {
            // No hardware claim exists for this fixture.
            core::mem::forget(claim);
            completions += 1;
            assert!(replaced.load(Ordering::Acquire));
            assert!(!original_completion.is_completed());
            assert!(replacement_completion.is_completed());
        })
    };
    let stored = IRQ_STATE.lock().descs.remove(&virq);
    assert!(
        stored
            .as_ref()
            .is_some_and(|line| Arc::ptr_eq(line, &replacement))
    );
    assert_eq!(resolved, Some(virq));
    assert_eq!(completions, 1);
    assert!(!original.lock().is_running);
    assert!(original_completion.is_completed());
    assert!(!replacement.lock().is_running);
    assert!(replacement_completion.is_completed());
}

#[def_test(serial)]
fn test_running_completion_reinitializes_between_dispatches() {
    let virq = 0x3140;
    let mut entry = IrqStateDesc::test_with_runtime(
        IrqDesc::from_virq(virq),
        Some(IrqAction::regular(Arc::new(test_handler))),
    );

    assert!(entry.execution_idle_completion().is_completed());

    entry.begin_running_dispatch();
    assert!(!entry.execution_idle_completion().is_completed());
    let wait_set = entry
        .finish_running_dispatch()
        .expect("running dispatch should produce completion wake");
    assert!(entry.execution_idle_completion().is_completed());
    wait_set.wake();

    entry.begin_running_dispatch();
    assert!(!entry.execution_idle_completion().is_completed());
    let wait_set = entry
        .finish_running_dispatch()
        .expect("second running dispatch should produce completion wake");
    assert!(entry.execution_idle_completion().is_completed());
    wait_set.wake();
}

#[def_test(serial)]
fn test_running_completion_old_wake_does_not_complete_next_dispatch() {
    let virq = 0x3143;
    let mut entry = IrqStateDesc::test_with_runtime(
        IrqDesc::from_virq(virq),
        Some(IrqAction::regular(Arc::new(test_handler))),
    );

    entry.begin_running_dispatch();
    let old_wait_set = entry
        .finish_running_dispatch()
        .expect("running dispatch should produce completion wake");
    assert!(entry.execution_idle_completion().is_completed());

    entry.begin_running_dispatch();
    assert!(!entry.execution_idle_completion().is_completed());
    old_wait_set.wake();
    assert!(
        !entry.execution_idle_completion().is_completed(),
        "a delayed wake from the previous running generation must not mark the next one done"
    );

    let next_wait_set = entry
        .finish_running_dispatch()
        .expect("next dispatch should produce its own completion wake");
    assert!(entry.execution_idle_completion().is_completed());
    next_wait_set.wake();
}

#[def_test(serial)]
fn test_running_completion_wakes_multiple_waiters() {
    let virq = 0x3141;
    let first_wake = CountWake(AtomicUsize::new(0));
    let second_wake = CountWake(AtomicUsize::new(0));
    let mut entry = IrqStateDesc::test_with_runtime(
        IrqDesc::from_virq(virq),
        Some(IrqAction::regular(Arc::new(test_handler))),
    );

    entry.begin_running_dispatch();
    let wait_source = entry.execution_idle_completion();
    let mut park_second = || {
        let deferred = entry
            .finish_running_dispatch()
            .expect("running dispatch should produce completion wake");
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
fn test_free_irq_removes_action_but_retains_descriptor() {
    let virq = 0x3115;
    let _binding = SyntheticLine::new(IrqDesc::from_virq(virq));
    assert_eq!(try_map(IrqDesc::from_virq(virq)), Ok(virq));

    assert!(try_register(virq, Arc::new(test_handler)).expect("register should work"));
    assert!(
        try_free_irq(virq)
            .expect("free_irq should not fail")
            .is_some()
    );
    let removed = IRQ_STATE.lock().remove_if_unused(virq);
    assert!(removed.is_some());
}

#[def_test(serial)]
fn test_free_irq_waits_for_running_dispatch() {
    check_free_waits_for_running_dispatch(0x312e);
}

#[def_test(serial)]
fn percpu_action_removal_does_not_apply_a_local_controller_plan() {
    let virq = 0x61d1;
    let handler: crate::Handler = Arc::new(|_| IrqEvent::HANDLED);
    let probe = LocalFlowProbe::new(virq, handler.clone());
    assert!(!probe.line.lock().has_enabled_local_cpu());
    let mut has_applied_plan = false;
    let retired = super::withdraw_action_on_line(virq, &probe.line, None, true, |_| {
        has_applied_plan = true;
    })
    .unwrap()
    .unwrap();
    // SAFETY: this serial fixture never starts a source or dispatch; the
    // detached node has no outstanding readers and cannot be reached again.
    let removed = unsafe { retired.reclaim() }.into_primary();
    assert!(!has_applied_plan);
    assert!(!probe.line.lock().has_enabled_local_cpu());
    assert_eq!(probe.line.lock().action_count(), 0);
    assert!(Arc::ptr_eq(&removed, &handler));
}

#[def_test(serial)]
fn test_free_percpu_irq_after_source_dispatch_and_completion_end() {
    let virq = 0x61d0;
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let handler: crate::Handler = Arc::new(move |_| {
        let line = IRQ_STATE.lock().descs.get(&virq).unwrap();
        assert!(!line.lock().is_running);
        observed.fetch_add(1, Ordering::Relaxed);
        IrqEvent::HANDLED
    });
    let probe = LocalFlowProbe::new(virq, handler.clone());
    let mut completions = 0;
    {
        let _pin = kspin::NoPreemptIrqSave::new();
        probe.set_enabled(true);
        assert_eq!(
            crate::dispatch::dispatch_claim(IrqRef::Virq(virq), || {
                assert!(!probe.line.lock().is_running);
                completions += 1;
            }),
            Some(virq)
        );
        probe.set_enabled(false);
    }
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(completions, 1);
    assert!(!probe.line.lock().has_enabled_local_cpu());
    IRQ_WAIT_TEST_TARGET.store(virq, Ordering::Release);
    IRQ_WAIT_TEST_OBSERVED.store(0, Ordering::Release);
    IRQ_WAIT_TEST_RELEASES.store(0, Ordering::Release);
    let old_hook = WAIT_FOR_RUNNING_HOOK
        .lock()
        .replace(test_release_running_on_wait);
    // SAFETY: only this task drives the synthetic source, and its synchronous
    // dispatch plus completion ended above. All local slots are disabled;
    // exclusive fixture ownership excludes restart and any concurrent caller.
    let result = unsafe { super::try_free_percpu_irq(virq) };
    *WAIT_FOR_RUNNING_HOOK.lock() = old_hook;
    IRQ_WAIT_TEST_TARGET.store(0, Ordering::Release);
    assert_eq!(IRQ_WAIT_TEST_OBSERVED.load(Ordering::Acquire), 0);
    assert_eq!(IRQ_WAIT_TEST_RELEASES.load(Ordering::Acquire), 0);
    let removed = result.unwrap().unwrap();
    assert!(Arc::ptr_eq(&removed, &handler));
    assert_eq!(Arc::strong_count(&handler), 2);
    drop(removed);
    assert_eq!(Arc::strong_count(&handler), 1);
    drop(probe);
}

fn check_free_waits_for_running_dispatch(virq: usize) {
    struct TrackedHandler;
    impl crate::IrqHandler for TrackedHandler {
        fn handle(&self, _: usize) -> IrqEvent {
            IrqEvent::HANDLED
        }
    }
    impl Drop for TrackedHandler {
        fn drop(&mut self) {
            IRQ_WAIT_OWNER_DROPS.fetch_add(1, Ordering::Release);
        }
    }
    fn finish_claim_after_observing_owner(virq: usize, active: usize) {
        if IRQ_WAIT_TEST_TARGET.load(Ordering::Acquire) != virq {
            return;
        }
        // The production free path retains the detached owner while draining
        // the original Line. Model completion outstanding after traversal.
        IRQ_WAIT_OWNER_RETAINED.store(
            IRQ_WAIT_OWNER_DROPS.load(Ordering::Acquire) == 0,
            Ordering::Release,
        );
        test_release_running_on_wait(virq, active);
    }
    let _binding = SyntheticLine::new(IrqDesc::from_virq(virq));
    assert_eq!(try_map(IrqDesc::from_virq(virq)), Ok(virq));
    IRQ_WAIT_TEST_TARGET.store(virq, Ordering::Release);
    IRQ_WAIT_TEST_OBSERVED.store(0, Ordering::Release);
    IRQ_WAIT_TEST_RELEASES.store(0, Ordering::Release);
    IRQ_WAIT_OWNER_DROPS.store(0, Ordering::Release);
    IRQ_WAIT_OWNER_RETAINED.store(false, Ordering::Release);
    let old_hook = WAIT_FOR_RUNNING_HOOK
        .lock()
        .replace(finish_claim_after_observing_owner);

    assert!(try_register(virq, Arc::new(TrackedHandler)).expect("register should work"));
    {
        let state = IRQ_STATE.lock();
        let retained_line = state.descs.get(&virq).expect("descriptor must exist");
        let mut entry = retained_line.lock();
        entry.begin_running_dispatch();
    }

    let handler = try_free_irq(virq).expect("free should not fail");

    *WAIT_FOR_RUNNING_HOOK.lock() = old_hook;
    IRQ_WAIT_TEST_TARGET.store(0, Ordering::Release);
    assert_eq!(IRQ_WAIT_TEST_OBSERVED.load(Ordering::Acquire), 1);
    assert_eq!(IRQ_WAIT_TEST_RELEASES.load(Ordering::Acquire), 1);
    assert!(IRQ_WAIT_OWNER_RETAINED.load(Ordering::Acquire));
    assert!(handler.is_some());
    assert_eq!(IRQ_WAIT_OWNER_DROPS.load(Ordering::Acquire), 0);
    drop(handler);
    assert_eq!(IRQ_WAIT_OWNER_DROPS.load(Ordering::Acquire), 1);
    let removed = IRQ_STATE.lock().remove_if_unused(virq);
    assert!(removed.is_some());
}

#[def_test(serial)]
fn test_free_percpu_irq_rejects_wrong_flow_without_mutation() {
    let virq = 0x61d1;
    let _binding = SyntheticLine::new(IrqDesc::from_virq(virq));
    assert_eq!(try_map(IrqDesc::from_virq(virq)), Ok(virq));
    let handler: crate::Handler = Arc::new(test_handler);
    assert!(try_register_disabled(virq, handler.clone()).unwrap());
    let line = IRQ_STATE.lock().descs.get(&virq).unwrap().clone();
    let (desc, flow, depth) = {
        let entry = line.lock();
        (entry.desc, entry.flow, entry.disable_depth_for_tests())
    };
    // SAFETY: this exclusively owned disabled ordinary fixture has never
    // dispatched, has no producer and cannot restart during the rejected call.
    let rejected = unsafe { super::try_free_percpu_irq(virq) };
    assert!(matches!(
        rejected,
        Err(IrqDescError::UnsupportedFlow { hwirq }) if hwirq == desc.hwirq
    ));
    {
        let entry = line.lock();
        assert_eq!(entry.desc, desc);
        assert_eq!(entry.flow, flow);
        assert_eq!(entry.disable_depth_for_tests(), depth);
        assert_eq!(entry.action_count(), 1);
        assert!(!entry.is_teardown_in_progress());
        assert!(!entry.has_active_execution());
    }
    assert_eq!(Arc::strong_count(&handler), 2);
    let removed = try_free_irq(virq).unwrap().unwrap();
    assert!(Arc::ptr_eq(&removed, &handler));
    assert!(retire_unused_test_line(virq));
}

#[def_test(serial)]
fn test_free_percpu_irq_does_not_remove_shared_action() {
    let virq = 0x61d2;
    let _binding = SyntheticLine::new(IrqDesc::from_virq(virq));
    assert_eq!(try_map(IrqDesc::from_virq(virq)), Ok(virq));
    let handler: crate::Handler = Arc::new(test_handler);
    let line = IRQ_STATE.lock().descs.get(&virq).unwrap().clone();
    let token = {
        let mut entry = line.lock();
        entry.set_flow_for_tests(crate::IrqFlow::PerCpu);
        entry.install_shared_action(handler.clone()).unwrap()
    };
    // SAFETY: the synthetic source has no producer or admitted execution;
    // all slots are disabled and exclusive fixture ownership prevents restart.
    let removed = unsafe { super::try_free_percpu_irq(virq) }.unwrap();
    assert!(removed.is_none());
    assert_eq!(line.lock().action_count(), 1);
    assert!(!line.lock().is_teardown_in_progress());
    assert_eq!(Arc::strong_count(&handler), 2);
    assert!(matches!(
        try_free_irq_action(virq, token),
        Err(IrqDescError::UnsupportedFlow { hwirq }) if hwirq == virq
    ));
    // This deliberately unsupported shared PerCpu fixture has no public
    // retirement route. It owns no readers, so private cleanup is sufficient.
    let removed = super::take_from_line(&line, Some(token))
        .unwrap()
        .into_primary();
    assert!(Arc::ptr_eq(&removed, &handler));
    assert!(retire_unused_test_line(virq));
}

fn check_management_isolation_on_wait(virq: usize, active: usize) {
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
            usize::from(entry.is_running),
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
    test_release_running_on_wait(virq, active);
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
    let _other_binding = SyntheticLine::new(IrqDesc::from_virq(IRQ_MANAGEMENT_OTHER_VIRQ));
    let _binding = SyntheticLine::new(desc);
    assert_eq!(
        super::try_map(IrqDesc::from_virq(IRQ_MANAGEMENT_OTHER_VIRQ)),
        Ok(IRQ_MANAGEMENT_OTHER_VIRQ)
    );
    assert_eq!(try_map(desc), Ok(virq));
    assert!(try_register_disabled(desc, Arc::new(test_handler)).unwrap());
    let line = IRQ_STATE.lock().descs.get(&virq).unwrap().clone();
    line.lock().begin_running_dispatch();
    let old_hook = WAIT_FOR_RUNNING_HOOK
        .lock()
        .replace(check_management_isolation_on_wait);

    let removed = try_free_irq(virq);

    *WAIT_FOR_RUNNING_HOOK.lock() = old_hook;
    IRQ_WAIT_TEST_TARGET.store(0, Ordering::Release);
    let has_retained_identity = IRQ_STATE
        .lock()
        .descs
        .get(&virq)
        .is_some_and(|stored| Arc::ptr_eq(&stored, &line));
    let registered_after_free = try_register_disabled(desc, Arc::new(test_handler));
    let final_removed = try_free_irq(virq);
    let other_removed = try_free_irq(IRQ_MANAGEMENT_OTHER_VIRQ);
    let retired = IRQ_STATE.lock().remove_if_unused(virq);
    let other_retired = IRQ_STATE.lock().remove_if_unused(IRQ_MANAGEMENT_OTHER_VIRQ);
    // Report only after restoring the hook and withdrawing both test actions.
    assert!(removed.unwrap().is_some());
    assert!(has_retained_identity);
    assert!(retired.is_some());
    assert!(other_retired.is_some());
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

fn replace_drained_instance_on_wait(virq: usize, _active: usize) {
    if IRQ_WAIT_TEST_TARGET.load(Ordering::Acquire) != virq {
        return;
    }
    let pass = IRQ_WAIT_TEST_RELEASES.fetch_add(1, Ordering::AcqRel);
    let replacement = (pass == 0).then(|| {
        let mut entry = IrqStateDesc::test_with_runtime(
            IrqDesc::from_virq(virq),
            Some(IrqAction::regular(Arc::new(test_handler))),
        );
        entry.begin_running_dispatch();
        Arc::new(IrqLine::new(entry))
    });
    let retained_line = IRQ_STATE
        .lock()
        .descs
        .get(&virq)
        .expect("waiting instance exists");
    let wake = retained_line
        .lock()
        .finish_running_dispatch()
        .expect("one synthetic dispatch remains");
    let removed_action = if pass == 0 {
        let action = super::take_from_line(&retained_line, None);
        let mut state = IRQ_STATE.lock();
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
    wake.wake();
    drop(removed_action);
}

#[def_test(serial)]
fn synchronization_waits_for_running_owner() {
    const VIRQ: usize = 0x5f69;
    static OBSERVED: AtomicUsize = AtomicUsize::new(0);
    fn finish_running_on_wait(virq: usize, active: usize) {
        if virq != VIRQ {
            return;
        }
        assert_eq!(active, 1);
        let line = IRQ_STATE.lock().descs.get(&virq).unwrap().clone();
        let wake = {
            let mut entry = line.lock();
            assert!(entry.is_running);
            OBSERVED.fetch_add(1, Ordering::Relaxed);
            entry.finish_running_dispatch().unwrap()
        };
        wake.wake();
    }

    let line = Arc::new(super::IrqLine::new(super::IrqStateDesc::test_new(
        IrqDesc::from_virq(VIRQ),
    )));
    line.lock().begin_running_dispatch();
    assert!(IRQ_STATE.lock().descs.insert(VIRQ, line.clone()).is_none());
    OBSERVED.store(0, Ordering::Relaxed);
    let old_hook = WAIT_FOR_RUNNING_HOOK.lock().replace(finish_running_on_wait);
    super::IrqDrain::new(VIRQ, &line).wait();
    *WAIT_FOR_RUNNING_HOOK.lock() = old_hook;
    let removed = IRQ_STATE.lock().descs.remove(&VIRQ);
    drop(removed);
    assert_eq!(OBSERVED.load(Ordering::Relaxed), 1);
    assert!(!line.lock().has_active_execution());
}

fn check_wait_keeps_original_instance(wait_fn: impl FnOnce(usize) -> Result<bool, IrqDescError>) {
    let virq = 0x3152;
    let _binding = SyntheticLine::new(IrqDesc::from_virq(virq));
    assert_eq!(try_map(IrqDesc::from_virq(virq)), Ok(virq));
    assert!(try_register(virq, Arc::new(test_handler)).unwrap());
    IRQ_STATE
        .lock()
        .descs
        .get(&virq)
        .unwrap()
        .lock()
        .begin_running_dispatch();
    IRQ_WAIT_TEST_TARGET.store(virq, Ordering::Release);
    IRQ_WAIT_TEST_RELEASES.store(0, Ordering::Release);
    let old_hook = WAIT_FOR_RUNNING_HOOK
        .lock()
        .replace(replace_drained_instance_on_wait);

    let result = wait_fn(virq);

    *WAIT_FOR_RUNNING_HOOK.lock() = old_hook;
    IRQ_WAIT_TEST_TARGET.store(0, Ordering::Release);
    let passes = IRQ_WAIT_TEST_RELEASES.load(Ordering::Acquire);
    let (replacement_active, wake) = {
        let state = IRQ_STATE.lock();
        let retained_line = state.descs.get(&virq).unwrap();
        let mut entry = retained_line.lock();
        let active = usize::from(entry.is_running);
        let wake = (active != 0).then(|| entry.finish_running_dispatch().unwrap());
        (active, wake)
    };
    if let Some(wake) = wake {
        wake.wake();
    }
    assert!(unregister(virq).is_some());
    assert!(retire_unused_test_line(virq));

    assert_eq!(result, Ok(true));
    assert_eq!(passes, 1, "wait must not follow the reused IRQ number");
    assert_eq!(
        replacement_active, 1,
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
    let line = Arc::new(IrqLine::new(IrqStateDesc::test_new(IrqDesc::from_virq(
        virq,
    ))));
    let weak = Arc::downgrade(&line);
    let (drain, removed) = {
        let mut state = IRQ_STATE.lock();
        assert!(!state.descs.contains_key(&virq));
        state.descs.insert(virq, line.clone());
        let drain = super::IrqDrain::new(virq, &state.descs.get(&virq).unwrap());
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
    let _binding = SyntheticLine::new(IrqDesc::from_virq(virq));
    assert_eq!(try_map(IrqDesc::from_virq(virq)), Ok(virq));

    assert!(try_register(virq, Arc::new(test_handler)).expect("register should work"));
    assert!(try_disable_irq(virq).expect("disable_irq should not fail"));
    {
        let state = IRQ_STATE.lock();
        let retained_line = state.descs.get(&virq).expect("descriptor must exist");
        let entry = retained_line.lock();
        assert_eq!(entry.disable_depth_for_tests(), 1);
        assert!(!entry.is_enabled_for_tests());
    }

    assert!(unregister(virq).is_some());
    assert!(retire_unused_test_line(virq));
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
    let _binding = SyntheticLine::new(IrqDesc::from_virq(virq));
    assert_eq!(try_map(IrqDesc::from_virq(virq)), Ok(virq));
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
        let retained_line = state
            .descs
            .get(&virq)
            .expect("action must remain installed");
        let entry = retained_line.lock();
        assert_eq!(entry.action_count(), 1);
        assert!(!entry.is_teardown_in_progress());
    }
    assert!(unregister(virq).is_some());
    assert!(retire_unused_test_line(virq));
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
fn test_synchronize_irq_waits_for_running_dispatch() {
    let virq = 0x312f;
    let _binding = SyntheticLine::new(IrqDesc::from_virq(virq));
    assert_eq!(try_map(IrqDesc::from_virq(virq)), Ok(virq));
    IRQ_WAIT_TEST_TARGET.store(virq, Ordering::Release);
    IRQ_WAIT_TEST_OBSERVED.store(0, Ordering::Release);
    IRQ_WAIT_TEST_RELEASES.store(0, Ordering::Release);
    let old_hook = WAIT_FOR_RUNNING_HOOK
        .lock()
        .replace(test_release_running_on_wait);

    assert!(try_register(virq, Arc::new(test_handler)).expect("register should work"));
    {
        let state = IRQ_STATE.lock();
        let retained_line = state.descs.get(&virq).expect("descriptor must exist");
        let mut entry = retained_line.lock();
        entry.begin_running_dispatch();
    }

    assert!(try_synchronize_irq(virq).expect("synchronize_irq should not fail"));

    *WAIT_FOR_RUNNING_HOOK.lock() = old_hook;
    IRQ_WAIT_TEST_TARGET.store(0, Ordering::Release);
    assert_eq!(IRQ_WAIT_TEST_OBSERVED.load(Ordering::Acquire), 1);
    assert_eq!(IRQ_WAIT_TEST_RELEASES.load(Ordering::Acquire), 1);
    {
        let state = IRQ_STATE.lock();
        let retained_line = state.descs.get(&virq).expect("descriptor must remain");
        let entry = retained_line.lock();
        assert!(!entry.is_running);
    }
    assert!(unregister(virq).is_some());
    assert!(retire_unused_test_line(virq));
}

#[def_test]
fn platform_plan_discard_leaves_enable_and_configuration_retryable() {
    let mut entry = IrqStateDesc::test_new(IrqDesc::from_virq(0x3160));
    {
        let abandoned = entry.prepare_auto_enable();
        assert!(abandoned.configure);
        assert_eq!(abandoned.enable, Some(true));
        assert_eq!(entry.configured_generation_for_tests(), None);
        assert!(!entry.is_enabled_for_tests());
        assert_eq!(entry.disable_depth_for_tests(), 0);
    }
    let retry = entry.prepare_auto_enable();
    assert!(retry.configure);
    assert_eq!(retry.enable, Some(true));
    entry.commit_platform_plan(retry);
    assert_eq!(entry.configured_generation_for_tests(), Some(0));
    assert!(entry.is_enabled_for_tests());
    let next = entry.prepare_enable_irq();
    assert!(!next.configure);
    assert_eq!(next.enable, None);
}

#[def_test]
fn platform_plan_nested_depth_changes_only_at_commit() {
    let mut entry = IrqStateDesc::test_new(IrqDesc::from_virq(0x3161));
    let initial = entry.prepare_auto_enable();
    entry.commit_platform_plan(initial);
    for depth in 0..2 {
        let plan = entry.prepare_disable_irq_nosync();
        assert_eq!(entry.disable_depth_for_tests(), depth);
        assert_eq!(entry.is_enabled_for_tests(), depth == 0);
        assert_eq!(plan.enable, (depth == 0).then_some(false));
        let retry = entry.prepare_disable_irq_nosync();
        assert_eq!(retry.enable, plan.enable);
        entry.commit_platform_plan(retry);
        assert_eq!(entry.disable_depth_for_tests(), depth + 1);
        assert!(!entry.is_enabled_for_tests());
    }
    for depth in [2, 1] {
        let plan = entry.prepare_enable_irq();
        assert_eq!(entry.disable_depth_for_tests(), depth);
        assert!(!entry.is_enabled_for_tests());
        assert!(!plan.configure);
        assert_eq!(plan.enable, (depth == 1).then_some(true));
        let retry = entry.prepare_enable_irq();
        assert_eq!(retry.enable, plan.enable);
        entry.commit_platform_plan(retry);
        assert_eq!(entry.disable_depth_for_tests(), depth - 1);
        assert_eq!(entry.is_enabled_for_tests(), depth == 1);
    }
}

#[def_test]
fn register_disabled_and_reconfigure_plans_are_side_effect_free() {
    let virq = 0x3162;
    let mut entry = IrqStateDesc::test_new(IrqDesc::from_virq(virq));
    let disabled = entry.prepare_register_disabled();
    assert_eq!(entry.disable_depth_for_tests(), 0);
    assert!(!disabled.configure);
    assert_eq!(disabled.enable, None);
    entry.commit_platform_plan(disabled);
    assert_eq!(entry.disable_depth_for_tests(), 1);
    let configure = entry.prepare_reconfigure_if_stale();
    assert!(configure.configure);
    assert_eq!(entry.configured_generation_for_tests(), None);
    let retry = entry.prepare_reconfigure_if_stale();
    assert!(retry.configure);
    entry.commit_platform_plan(retry);
    assert_eq!(entry.configured_generation_for_tests(), Some(0));
    assert!(!entry.is_enabled_for_tests());
    assert_eq!(entry.disable_depth_for_tests(), 1);
    entry.update_desc(IrqDesc::from_virq(virq).with_controller(IrqController::Gic));
    let stale = entry.prepare_reconfigure_if_stale();
    assert!(stale.configure);
    assert_eq!(entry.configured_generation_for_tests(), Some(0));
    assert!(entry.prepare_reconfigure_if_stale().configure);
    entry.commit_platform_plan(stale);
    assert_eq!(entry.configured_generation_for_tests(), Some(1));
    assert!(!entry.is_enabled_for_tests());
    assert_eq!(entry.disable_depth_for_tests(), 1);
}

#[def_test]
fn test_legacy_enable_forces_platform_enable_when_already_enabled() {
    let virq = 0x3114;
    let mut entry = IrqStateDesc::test_new(IrqDesc::from_virq(virq));

    let first = entry.prepare_auto_enable();
    assert_eq!(first.enable, Some(true));
    assert!(!entry.is_enabled_for_tests());
    entry.commit_platform_plan(first);
    assert!(entry.is_enabled_for_tests());

    let depth_aware = entry.prepare_enable_irq();
    assert_eq!(depth_aware.enable, None);

    let legacy = entry.prepare_legacy_enable();
    assert_eq!(legacy.enable, Some(true));
}

#[def_test]
fn test_reconfigure_if_stale_does_not_toggle_enabled_state() {
    let virq = 0x3142;
    let mut entry = IrqStateDesc::test_new(IrqDesc::from_virq(virq));

    let first = entry.prepare_auto_enable();
    assert!(first.configure);
    assert_eq!(first.enable, Some(true));
    assert_eq!(entry.configured_generation_for_tests(), None);
    assert!(!entry.is_enabled_for_tests());
    entry.commit_platform_plan(first);
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
    assert_eq!(entry.configured_generation_for_tests(), Some(0));
    entry.commit_platform_plan(stale);
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
        let mut entry = line.lock();
        let plan = entry.prepare_auto_enable();
        entry.commit_platform_plan(plan);
    }
    {
        let mut state = IRQ_STATE.lock();
        state.descs.insert(virq, line.clone());
    }

    let pending = PendingIrq::simulated(IrqRef::Virq(virq));
    super::dispatch_simulated_irq(pending);

    assert_eq!(REGULAR_CALLS.load(Ordering::Relaxed), 1);
    assert!(unregister(virq).is_some());
    assert!(retire_unused_test_line(virq));
}

#[def_test(serial)]
fn test_irq_handler_receives_resolved_virq() {
    let virq = 0x3104;
    let _binding = SyntheticLine::new(IrqDesc::from_virq(virq));
    assert_eq!(try_map(IrqDesc::from_virq(virq)), Ok(virq));
    HANDLER_OBSERVED_IRQ.store(0, Ordering::Relaxed);

    assert!(try_register(virq, Arc::new(test_observe_irq_handler)).unwrap());
    let pending = PendingIrq::simulated(IrqRef::Virq(virq));
    super::dispatch_simulated_irq(pending);

    assert_eq!(HANDLER_OBSERVED_IRQ.load(Ordering::Relaxed), virq);
    assert!(unregister(virq).is_some());
    assert!(retire_unused_test_line(virq));
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
        let mut entry = line.lock();
        let plan = entry.prepare_auto_enable();
        entry.commit_platform_plan(plan);
    }
    {
        let mut state = IRQ_STATE.lock();
        state.descs.insert(virq, line.clone());
    }

    let pending = PendingIrq::simulated(IrqRef::Virq(virq));
    super::dispatch_simulated_irq(pending);

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
    assert!(retire_unused_test_line(virq));
}

#[def_test(serial)]
fn test_unregister_keeps_msi_descriptor_for_free_msix() {
    // Exercise MSI ownership bookkeeping without claiming a real APIC vector.
    let domain = crate::domain::TEST_DOMAIN;
    let (hwirq, virq) =
        unused_msi_test_mapping(&IRQ_STATE.lock()).expect("MSI test mapping space exhausted");
    let desc = IrqDesc::new(hwirq, IrqTrigger::EdgeRising)
        .with_domain(domain)
        .with_flags(IrqFlags::MSI)
        .with_virq(virq);
    let prepared = Arc::new(IrqLine::new(IrqStateDesc::test_new(desc)));
    let action = IrqAction::regular(Arc::new(test_handler));
    {
        let mut state = IRQ_STATE.lock();
        assert!(unused_msi_test_hwirq(&state, hwirq));
        assert!(!state.descs.contains_key(&virq));
        let desc = state
            .try_resolve_desc(desc, Some(&prepared))
            .expect("MSI descriptor should be accepted")
            .into_desc();
        let retained_line = state
            .descs
            .get(&desc.logical_irq().unwrap())
            .expect("descriptor state must exist after try_resolve_desc");
        let mut entry = retained_line.lock();
        assert!(entry.install_regular_action(action));
    }

    assert!(unregister(virq).is_some());
    {
        let state = IRQ_STATE.lock();
        let retained_line = state.descs.get(&virq).expect("MSI descriptor must stay");
        let entry = retained_line.lock();
        assert!(entry.is_unused());
        assert_eq!(state.translated_hwirq(domain, hwirq), Some(virq));
    }
    {
        let mut state = IRQ_STATE.lock();
        assert_eq!(state.remove_msi_if_unused(virq), Some(hwirq));
        assert_eq!(state.translated_hwirq(domain, hwirq), None);
        assert!(!state.descs.contains_key(&virq));
    }

    let new_hwirq = {
        let state = IRQ_STATE.lock();
        unused_msi_test_hwirq_except(&state, hwirq).expect("MSI test hwirq space exhausted")
    };
    let remapped = SyntheticLine::mapped(
        IrqDesc::new(new_hwirq, IrqTrigger::EdgeRising)
            .with_domain(domain)
            .with_flags(IrqFlags::MSI)
            .with_virq(virq),
    );
    let remapped_virq = remapped.virq();
    assert_eq!(remapped_virq, virq);
    {
        let mut state = IRQ_STATE.lock();
        assert_eq!(state.remove_msi_if_unused(virq), Some(new_hwirq));
        assert_eq!(state.translated_hwirq(domain, new_hwirq), None);
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Minimal fixed-vector softirq foundation.

#[cfg(any(unittest, feature = "irq_stat"))]
use core::sync::atomic::AtomicUsize;
use core::sync::atomic::Ordering;

use kcpu_slot::{CpuPin, cpu_slot};
use kspin::{NoPreempt, NoPreemptIrqSave, SpinNoIrq, static_lock};

use crate::{
    context::{self, SoftIrqContextGuard},
    deferred::{DeferredExecutorHooks, DeferredRunContext, register_deferred_executor},
};

/// Softirq callback function.
///
/// Actions run in IRQ-tail, BH-enable, or `ksoftirqd` drain context with
/// preemption disabled. The softirq runner enters and exits with local IRQs
/// masked, but enables local IRQs while invoking actions, matching Linux's
/// `__do_softirq()` action-loop contract. Actions must not sleep, allocate on
/// the hot path, or depend on process context.
pub type SoftirqAction = fn();

/// Scheduler-facing wake bridge for per-CPU softirq daemon threads.
///
/// `kirq` owns per-CPU pending bits and the decision to defer execution. The
/// task layer owns the sleepable daemon task that drains deferred work.
#[kiface::interface]
pub trait SoftirqDaemonIf {
    /// Reports a pending scheduling request for the current task.
    ///
    /// Called with local IRQs disabled and the CPU pinned. This query must not
    /// schedule, block, or consume the request. Return false before task setup
    /// and when kernel preemption is disabled by configuration.
    fn needs_resched() -> bool;

    /// Wakes the daemon serving the current CPU's pending softirq state.
    ///
    /// Implementations must be callable from IRQ-disabled context.
    fn wake_current_cpu();
}

/// Installs the softirq hardirq-exit runner.
///
/// Returns `false` if another deferred IRQ-tail executor is already installed.
/// The softirq subsystem is intended to be the single owner of the generic
/// deferred executor; workerqueue, tasklet, and threaded IRQ work should hang
/// from softirq-specific APIs in later milestones.
pub fn init() -> bool {
    register_deferred_executor(DeferredExecutorHooks {
        on_hardirq_exit: Some(run_hardirq_exit_softirqs),
    })
}

/// Fixed softirq vector identifiers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(usize)]
pub enum SoftirqVec {
    /// High-priority bottom-half work.
    High    = 0,
    /// Timer bottom-half work.
    Timer   = 1,
    /// Network transmit bottom-half work.
    NetTx   = 2,
    /// Network receive bottom-half work.
    NetRx   = 3,
    /// Block I/O bottom-half work.
    Block   = 4,
    /// IRQ polling bottom-half work.
    IrqPoll = 5,
    /// Compatibility slot for future tasklet-style work.
    Tasklet = 6,
    /// Scheduler bottom-half work.
    Sched   = 7,
    /// High-resolution timer bottom-half work.
    Hrtimer = 8,
    /// RCU bottom-half work.
    Rcu     = 9,
}

impl SoftirqVec {
    /// Number of supported fixed softirq vectors.
    pub const COUNT: usize = 10;

    /// Returns the vector as a stable table index.
    #[inline]
    pub const fn as_usize(self) -> usize {
        self as usize
    }

    #[inline]
    const fn bit(self) -> usize {
        1usize << self.as_usize()
    }

    #[inline]
    const fn from_index(index: usize) -> Option<Self> {
        match index {
            0 => Some(Self::High),
            1 => Some(Self::Timer),
            2 => Some(Self::NetTx),
            3 => Some(Self::NetRx),
            4 => Some(Self::Block),
            5 => Some(Self::IrqPoll),
            6 => Some(Self::Tasklet),
            7 => Some(Self::Sched),
            8 => Some(Self::Hrtimer),
            9 => Some(Self::Rcu),
            _ => None,
        }
    }
}

const SOFTIRQ_MAX_RESTART: usize = 10;
const SOFTIRQ_VALID_MASK: usize = (1usize << SoftirqVec::COUNT) - 1;

cpu_slot! {
    static SOFTIRQ_PENDING: core::sync::atomic::AtomicUsize =
        core::sync::atomic::AtomicUsize::new(0);
}

static_lock! {
    static SOFTIRQ_ACTIONS: SpinNoIrq<[Option<SoftirqAction>; SoftirqVec::COUNT]> =
        SpinNoIrq::new([None; SoftirqVec::COUNT]);
}

#[cfg(unittest)]
static SOFTIRQ_DAEMON_WAKE_ENABLED: AtomicUsize = AtomicUsize::new(1);

#[cfg(any(unittest, feature = "irq_stat"))]
struct SoftirqCpuStats {
    context_leak_warnings: AtomicUsize,
    restart_limit_hits: AtomicUsize,
    unhandled_vector_warnings: AtomicUsize,
    runs: AtomicUsize,
    daemon_wake_requests: AtomicUsize,
    daemon_wake_suppressed_context: AtomicUsize,
    context_deferred_runs: AtomicUsize,
    irqoff_misuse_warnings: AtomicUsize,
}

#[cfg(any(unittest, feature = "irq_stat"))]
impl SoftirqCpuStats {
    const fn new() -> Self {
        Self {
            context_leak_warnings: AtomicUsize::new(0),
            restart_limit_hits: AtomicUsize::new(0),
            unhandled_vector_warnings: AtomicUsize::new(0),
            runs: AtomicUsize::new(0),
            daemon_wake_requests: AtomicUsize::new(0),
            daemon_wake_suppressed_context: AtomicUsize::new(0),
            context_deferred_runs: AtomicUsize::new(0),
            irqoff_misuse_warnings: AtomicUsize::new(0),
        }
    }

    fn add_to(&self, diagnostics: &mut SoftirqDiagnostics) {
        diagnostics.context_leak_warnings += self.context_leak_warnings.load(Ordering::Relaxed);
        diagnostics.restart_limit_hits += self.restart_limit_hits.load(Ordering::Relaxed);
        diagnostics.unhandled_vector_warnings +=
            self.unhandled_vector_warnings.load(Ordering::Relaxed);
        diagnostics.runs += self.runs.load(Ordering::Relaxed);
        diagnostics.daemon_wake_requests += self.daemon_wake_requests.load(Ordering::Relaxed);
        diagnostics.daemon_wake_suppressed_context +=
            self.daemon_wake_suppressed_context.load(Ordering::Relaxed);
        diagnostics.context_deferred_runs += self.context_deferred_runs.load(Ordering::Relaxed);
        diagnostics.irqoff_misuse_warnings += self.irqoff_misuse_warnings.load(Ordering::Relaxed);
    }

    fn clear(&self) {
        self.context_leak_warnings.store(0, Ordering::Relaxed);
        self.restart_limit_hits.store(0, Ordering::Relaxed);
        self.unhandled_vector_warnings.store(0, Ordering::Relaxed);
        self.runs.store(0, Ordering::Relaxed);
        self.daemon_wake_requests.store(0, Ordering::Relaxed);
        self.daemon_wake_suppressed_context
            .store(0, Ordering::Relaxed);
        self.context_deferred_runs.store(0, Ordering::Relaxed);
        self.irqoff_misuse_warnings.store(0, Ordering::Relaxed);
    }
}

#[cfg(any(unittest, feature = "irq_stat"))]
// SAFETY: every field is an `AtomicUsize`; copying the all-zero template
// creates independent atomic objects without ownership or drop state.
unsafe impl kcpu_slot::StaticSlotValue for SoftirqCpuStats {}

#[cfg(any(unittest, feature = "irq_stat"))]
cpu_slot! {
    static SOFTIRQ_STATS: crate::bottom_half::softirq::SoftirqCpuStats =
        crate::bottom_half::softirq::SoftirqCpuStats::new();
}

/// Softirq diagnostic counters.
#[cfg(any(unittest, feature = "irq_stat"))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct SoftirqDiagnostics {
    /// Number of softirq handlers that returned with changed context state.
    pub context_leak_warnings: usize,
    /// Number of runs that stopped because the restart limit was reached.
    pub restart_limit_hits: usize,
    /// Number of pending vectors observed without a registered action.
    pub unhandled_vector_warnings: usize,
    /// Number of softirq actions run.
    pub runs: usize,
    /// Number of wake requests made for the per-CPU softirq daemon.
    pub daemon_wake_requests: usize,
    /// Number of daemon wake attempts suppressed by interrupt-like context.
    pub daemon_wake_suppressed_context: usize,
    /// Number of runs deferred because the current context cannot serve softirq.
    pub context_deferred_runs: usize,
    /// Number of `raise_softirq_irqoff` calls observed with local IRQs enabled.
    pub irqoff_misuse_warnings: usize,
}

/// Result of a softirq run attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SoftirqRunResult {
    /// No pending softirq was present.
    NoPending,
    /// At least one softirq action ran.
    Ran,
    /// Pending work remains due to context, a scheduling request, or the restart limit.
    Deferred,
}

/// Registers a fixed softirq action.
///
/// This API is intended for init-time registration. It does not support
/// unregistering actions while the system is running.
pub fn open_softirq(vec: SoftirqVec, action: SoftirqAction) -> bool {
    if !open_softirqs(&[(vec, action)]) {
        warn!("softirq vector {} already registered", vec.as_usize());
        return false;
    }
    true
}

/// Registers a group of fixed softirq actions atomically at initialization.
///
/// Returns false without modifying any slot if an entry repeats a vector or
/// any requested vector is already occupied. An empty group succeeds unchanged.
/// At most [`SoftirqVec::COUNT`] entries are accepted. Validation and publication
/// share the action-table lock, so dispatch snapshots cannot see a partial group.
/// No allocation, callback or pending-bit change occurs here.
///
/// Actions must be ready to run before registration. This does not publish the
/// caller's service readiness and does not provide runtime unregistration.
pub fn open_softirqs(entries: &[(SoftirqVec, SoftirqAction)]) -> bool {
    if entries.len() > SoftirqVec::COUNT {
        return false;
    }
    let mut actions = SOFTIRQ_ACTIONS.lock();
    let mut seen = 0;
    for &(vec, _) in entries {
        if seen & vec.bit() != 0 || actions[vec.as_usize()].is_some() {
            return false;
        }
        seen |= vec.bit();
    }
    for &(vec, action) in entries {
        actions[vec.as_usize()] = Some(action);
    }
    true
}

/// Returns whether a fixed softirq vector has an installed action.
#[cfg(unittest)]
pub fn is_softirq_open(vec: SoftirqVec) -> bool {
    SOFTIRQ_ACTIONS.lock()[vec.as_usize()].is_some()
}

/// Raises a softirq on the current CPU.
///
/// The function pins the current CPU and masks local IRQs while updating the
/// per-CPU pending bit. When the current CPU's pending mask transitions from
/// empty to non-empty in task context, it also wakes the current CPU's
/// `ksoftirqd`; hardirq, active softirq, and BH-disabled contexts suppress that
/// wake because their exit paths are responsible for draining or deferring the
/// pending work.
pub fn raise_softirq(vec: SoftirqVec) {
    let guard = NoPreemptIrqSave::new();
    if mark_softirq_pending(&guard, vec) {
        wake_softirqd_if_needed(&guard);
    }
}

/// Raises a softirq on the current CPU when local IRQs are already disabled.
///
/// The function still pins the current CPU before touching per-CPU state.
pub fn raise_softirq_irqoff(vec: SoftirqVec) {
    let guard = NoPreempt::new();

    #[cfg(any(unittest, feature = "irq_context_debug"))]
    let restore_irq_enabled = karch::local_irq_enabled();
    #[cfg(any(unittest, feature = "irq_context_debug"))]
    if restore_irq_enabled {
        let warning_count = record_irqoff_misuse_warning(&guard);
        warn!("raise_softirq_irqoff called with local IRQs enabled ({warning_count} warnings)");
        karch::disable_local_irq();
    }

    if mark_softirq_pending(&guard, vec) {
        wake_softirqd_if_needed(&guard);
    }

    #[cfg(any(unittest, feature = "irq_context_debug"))]
    if restore_irq_enabled {
        karch::enable_local_irq();
    }
}

/// Returns the current CPU's pending softirq bit mask.
pub fn local_softirq_pending() -> usize {
    let guard = NoPreemptIrqSave::new();
    SOFTIRQ_PENDING.current(&guard).load(Ordering::Acquire) & SOFTIRQ_VALID_MASK
}

/// Runs pending softirqs on the current CPU when context permits it.
pub fn run_pending_softirqs() -> SoftirqRunResult {
    let _guard = NoPreemptIrqSave::new();
    run_pending_softirqs_irqoff()
}

/// Runs pending softirqs with local IRQs already disabled.
///
/// The function is public within the crate for IRQ-tail and BH-enable paths.
pub(crate) fn run_pending_softirqs_irqoff() -> SoftirqRunResult {
    run_pending_softirqs_irqoff_with_resched(SoftirqDaemonIf::needs_resched)
}

fn run_pending_softirqs_irqoff_with_resched(needs_resched: impl Fn() -> bool) -> SoftirqRunResult {
    let guard = NoPreempt::new();
    let pending_slot = SOFTIRQ_PENDING.current(&guard);
    if pending_slot.load(Ordering::Acquire) & SOFTIRQ_VALID_MASK == 0 {
        return SoftirqRunResult::NoPending;
    }
    if !can_run_softirqs() {
        record_context_deferred_run(&guard);
        return SoftirqRunResult::Deferred;
    }

    let mut restarts_left = SOFTIRQ_MAX_RESTART;
    let mut ran_any = false;

    loop {
        let pending = pending_slot.swap(0, Ordering::Acquire) & SOFTIRQ_VALID_MASK;
        if pending == 0 {
            return if ran_any {
                SoftirqRunResult::Ran
            } else {
                SoftirqRunResult::NoPending
            };
        }

        run_pending_batch(&guard, pending);
        ran_any = true;

        restarts_left -= 1;
        let remaining = pending_slot.load(Ordering::Acquire) & SOFTIRQ_VALID_MASK;
        if remaining == 0 {
            return SoftirqRunResult::Ran;
        }
        if restarts_left == 0 {
            record_restart_limit_hit(&guard);
        }
        // Finish the current batch before yielding so even a busy daemon makes progress.
        if restarts_left == 0 || needs_resched() {
            wake_softirqd(&guard);
            return SoftirqRunResult::Deferred;
        }
    }
}

/// Returns current softirq diagnostics.
#[cfg(any(unittest, feature = "irq_stat"))]
pub(crate) fn softirq_diagnostics() -> SoftirqDiagnostics {
    let mut diagnostics = SoftirqDiagnostics::default();
    for cpu in 0..kbuild_config::NR_CPUS {
        if let Some(area) = kcpu_slot::area(kcpu_slot::CpuId::new(cpu)) {
            SOFTIRQ_STATS.at(&area).add_to(&mut diagnostics);
        }
    }
    diagnostics
}

/// Clears softirq diagnostics.
#[cfg(any(unittest, feature = "irq_stat"))]
pub(crate) fn clear_softirq_diagnostics() {
    for cpu in 0..kbuild_config::NR_CPUS {
        if let Some(area) = kcpu_slot::area(kcpu_slot::CpuId::new(cpu)) {
            SOFTIRQ_STATS.at(&area).clear();
        }
    }
}

#[cfg(unittest)]
/// Sets whether test builds call the real daemon wake provider.
pub fn set_softirq_daemon_wake_enabled_for_tests(enabled: bool) -> bool {
    SOFTIRQ_DAEMON_WAKE_ENABLED.swap(enabled as usize, Ordering::AcqRel) != 0
}

/// Replaces a softirq action and returns the previous action.
#[cfg(unittest)]
pub fn replace_softirq_action_for_tests(
    vec: SoftirqVec,
    action: Option<SoftirqAction>,
) -> Option<SoftirqAction> {
    let mut actions = SOFTIRQ_ACTIONS.lock();
    core::mem::replace(&mut actions[vec.as_usize()], action)
}

/// Returns whether a softirq vector currently owns the given action.
#[cfg(unittest)]
pub fn softirq_action_matches_for_tests(vec: SoftirqVec, action: SoftirqAction) -> bool {
    SOFTIRQ_ACTIONS.lock()[vec.as_usize()]
        .is_some_and(|installed| core::ptr::fn_addr_eq(installed, action))
}

/// Clears a pending softirq bit on the current CPU.
#[cfg(unittest)]
pub fn clear_softirq_pending_for_tests(vec: SoftirqVec) {
    let guard = NoPreemptIrqSave::new();
    SOFTIRQ_PENDING
        .current(&guard)
        .fetch_and(!vec.bit(), Ordering::Release);
}

unittest::path_mod_test!(pub mod test_support, "../../ut-tests/bottom_half_softirq_test_support.rs");

fn run_hardirq_exit_softirqs(_ctx: DeferredRunContext) {
    let _ = run_pending_softirqs_irqoff();
}

#[inline]
fn wake_softirqd_if_needed(pin: &impl CpuPin) {
    let snapshot = context::irq_context_snapshot_irqoff();
    if snapshot.is_in_interrupt_context() {
        record_daemon_wake_suppressed_context(pin);
        return;
    }
    wake_softirqd(pin);
}

#[inline]
fn mark_softirq_pending(pin: &impl CpuPin, vec: SoftirqVec) -> bool {
    let previous = SOFTIRQ_PENDING
        .current(pin)
        .fetch_or(vec.bit(), Ordering::Release);
    previous & SOFTIRQ_VALID_MASK == 0
}

#[inline]
fn wake_softirqd(pin: &impl CpuPin) {
    record_daemon_wake_request(pin);

    #[cfg(unittest)]
    if SOFTIRQ_DAEMON_WAKE_ENABLED.load(Ordering::Acquire) == 0 {
        return;
    }

    SoftirqDaemonIf::wake_current_cpu();
}

#[inline]
fn can_run_softirqs() -> bool {
    let snapshot = context::irq_context_snapshot_irqoff();
    !snapshot.is_in_interrupt_context()
}

/// Runs one batch of pending softirq vectors with local IRQs enabled.
///
/// `pin` is only used to select the current CPU's per-CPU slot area for the
/// `record_*` helpers. It is not a `CpuLocalGuard`: local IRQs are enabled
/// below, so an IRQ handler may re-enter and touch the same slots. That is
/// sound only because every slot reached through `pin` here holds an atomic
/// payload (`CpuSlot::current` requires `T: Sync`), which is safe for IRQ
/// re-entry. Any non-atomic slot access added to this region must re-acquire a
/// fresh pin after `enable_local_irq()` instead of reusing this one.
fn run_pending_batch(pin: &impl CpuPin, mut pending: usize) {
    let actions = *SOFTIRQ_ACTIONS.lock();
    let _softirq_context = SoftIrqContextGuard::enter_irqoff();
    karch::enable_local_irq();

    while pending != 0 {
        let index = pending.trailing_zeros() as usize;
        pending &= !(1usize << index);

        let Some(vec) = SoftirqVec::from_index(index) else {
            continue;
        };
        let Some(action) = actions[vec.as_usize()] else {
            record_unhandled_vector_warning(pin);
            warn!("softirq vector {} has no registered action", vec.as_usize());
            continue;
        };

        #[cfg(any(unittest, feature = "irq_context_debug"))]
        let before = context::irq_context_snapshot();
        action();

        #[cfg(any(unittest, feature = "irq_context_debug"))]
        {
            let after = context::irq_context_snapshot();
            if after != before {
                let warning_count = record_context_leak_warning(pin);
                warn!(
                    "softirq vector {} leaked context state: before={:?} after={:?} \
                     ({warning_count} warnings)",
                    vec.as_usize(),
                    before,
                    after
                );
                karch::disable_local_irq();
                context::restore_current_state_snapshot_irqoff(before);
                karch::enable_local_irq();
            }
        }
        record_softirq_run(pin);
    }

    karch::disable_local_irq();
}

#[cfg(any(unittest, feature = "irq_stat"))]
#[inline]
fn with_softirq_stats<R>(pin: &impl CpuPin, f: impl FnOnce(&SoftirqCpuStats) -> R) -> R {
    f(SOFTIRQ_STATS.current(pin))
}

#[cfg(any(unittest, feature = "irq_stat"))]
#[inline]
fn softirq_stat_inc(counter: &AtomicUsize) -> usize {
    counter.fetch_add(1, Ordering::Relaxed) + 1
}

#[inline]
fn record_softirq_run(pin: &impl CpuPin) {
    #[cfg(any(unittest, feature = "irq_stat"))]
    with_softirq_stats(pin, |stats| softirq_stat_inc(&stats.runs));

    #[cfg(not(any(unittest, feature = "irq_stat")))]
    let _ = pin;
}

#[inline]
fn record_restart_limit_hit(pin: &impl CpuPin) {
    #[cfg(any(unittest, feature = "irq_stat"))]
    with_softirq_stats(pin, |stats| softirq_stat_inc(&stats.restart_limit_hits));

    #[cfg(not(any(unittest, feature = "irq_stat")))]
    let _ = pin;
}

#[inline]
fn record_unhandled_vector_warning(pin: &impl CpuPin) {
    #[cfg(any(unittest, feature = "irq_stat"))]
    with_softirq_stats(pin, |stats| {
        softirq_stat_inc(&stats.unhandled_vector_warnings)
    });

    #[cfg(not(any(unittest, feature = "irq_stat")))]
    let _ = pin;
}

#[inline]
fn record_daemon_wake_request(pin: &impl CpuPin) {
    #[cfg(any(unittest, feature = "irq_stat"))]
    with_softirq_stats(pin, |stats| softirq_stat_inc(&stats.daemon_wake_requests));

    #[cfg(not(any(unittest, feature = "irq_stat")))]
    let _ = pin;
}

#[inline]
fn record_daemon_wake_suppressed_context(pin: &impl CpuPin) {
    #[cfg(any(unittest, feature = "irq_stat"))]
    with_softirq_stats(pin, |stats| {
        softirq_stat_inc(&stats.daemon_wake_suppressed_context)
    });

    #[cfg(not(any(unittest, feature = "irq_stat")))]
    let _ = pin;
}

#[inline]
fn record_context_deferred_run(pin: &impl CpuPin) {
    #[cfg(any(unittest, feature = "irq_stat"))]
    with_softirq_stats(pin, |stats| softirq_stat_inc(&stats.context_deferred_runs));

    #[cfg(not(any(unittest, feature = "irq_stat")))]
    let _ = pin;
}

#[cfg(any(unittest, feature = "irq_context_debug"))]
#[inline]
fn record_context_leak_warning(pin: &impl CpuPin) -> usize {
    #[cfg(any(unittest, feature = "irq_stat"))]
    {
        with_softirq_stats(pin, |stats| softirq_stat_inc(&stats.context_leak_warnings))
    }

    #[cfg(not(any(unittest, feature = "irq_stat")))]
    {
        1
    }
}

#[cfg(any(unittest, feature = "irq_context_debug"))]
#[inline]
fn record_irqoff_misuse_warning(pin: &impl CpuPin) -> usize {
    #[cfg(any(unittest, feature = "irq_stat"))]
    {
        with_softirq_stats(pin, |stats| softirq_stat_inc(&stats.irqoff_misuse_warnings))
    }

    #[cfg(not(any(unittest, feature = "irq_stat")))]
    {
        1
    }
}

unittest::path_mod_test!(pub mod tests_softirq, "../../ut-tests/bottom_half_softirq.rs");

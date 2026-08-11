// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Minimal fixed-vector softirq foundation.

use core::sync::atomic::{AtomicUsize, Ordering};

use kspin::{NoPreempt, NoPreemptIrqSave, SpinNoIrq};

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

#[percpu::def_percpu]
static SOFTIRQ_PENDING: AtomicUsize = AtomicUsize::new(0);

static SOFTIRQ_ACTIONS: SpinNoIrq<[Option<SoftirqAction>; SoftirqVec::COUNT]> =
    SpinNoIrq::new([None; SoftirqVec::COUNT]);

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
#[percpu::def_percpu]
static SOFTIRQ_STATS: SoftirqCpuStats = SoftirqCpuStats::new();

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
    /// Pending work exists but current context or restart limits prevented a full run.
    Deferred,
}

/// Registers a fixed softirq action.
///
/// This API is intended for init-time registration. It does not support
/// unregistering actions while the system is running.
pub fn open_softirq(vec: SoftirqVec, action: SoftirqAction) -> bool {
    let mut actions = SOFTIRQ_ACTIONS.lock();
    let slot = &mut actions[vec.as_usize()];
    if slot.is_some() {
        warn!("softirq vector {} already registered", vec.as_usize());
        return false;
    }
    *slot = Some(action);
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
    let _guard = NoPreemptIrqSave::new();
    if mark_softirq_pending(vec) {
        wake_softirqd_if_needed();
    }
}

/// Raises a softirq on the current CPU when local IRQs are already disabled.
///
/// The function still pins the current CPU before touching per-CPU state.
pub fn raise_softirq_irqoff(vec: SoftirqVec) {
    let _guard = NoPreempt::new();

    #[cfg(any(unittest, feature = "irq_context_debug"))]
    let restore_irq_enabled = karch::local_irq_enabled();
    #[cfg(any(unittest, feature = "irq_context_debug"))]
    if restore_irq_enabled {
        let warning_count = record_irqoff_misuse_warning();
        warn!("raise_softirq_irqoff called with local IRQs enabled ({warning_count} warnings)");
        karch::disable_local_irq();
    }

    if mark_softirq_pending(vec) {
        wake_softirqd_if_needed();
    }

    #[cfg(any(unittest, feature = "irq_context_debug"))]
    if restore_irq_enabled {
        karch::enable_local_irq();
    }
}

/// Returns the current CPU's pending softirq bit mask.
pub fn local_softirq_pending() -> usize {
    let _guard = NoPreemptIrqSave::new();
    pending_ref().load(Ordering::Acquire) & SOFTIRQ_VALID_MASK
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
    let _guard = NoPreempt::new();
    if pending_ref().load(Ordering::Acquire) & SOFTIRQ_VALID_MASK == 0 {
        return SoftirqRunResult::NoPending;
    }
    if !can_run_softirqs() {
        record_context_deferred_run();
        return SoftirqRunResult::Deferred;
    }

    let mut restarts_left = SOFTIRQ_MAX_RESTART;
    let mut ran_any = false;

    loop {
        let pending = pending_ref().swap(0, Ordering::Acquire) & SOFTIRQ_VALID_MASK;
        if pending == 0 {
            return if ran_any {
                SoftirqRunResult::Ran
            } else {
                SoftirqRunResult::NoPending
            };
        }

        run_pending_batch(pending);
        ran_any = true;

        restarts_left -= 1;
        if restarts_left == 0 {
            let remaining = pending_ref().load(Ordering::Acquire) & SOFTIRQ_VALID_MASK;
            if remaining != 0 {
                record_restart_limit_hit();
                wake_softirqd();
                return SoftirqRunResult::Deferred;
            }
            return SoftirqRunResult::Ran;
        }
    }
}

/// Returns current softirq diagnostics.
#[cfg(any(unittest, feature = "irq_stat"))]
pub(crate) fn softirq_diagnostics() -> SoftirqDiagnostics {
    let mut diagnostics = SoftirqDiagnostics::default();
    for cpu in 0..kbuild_config::NR_CPUS {
        // SAFETY: `cpu` is bounded by `NR_CPUS`, the same compile-time bound
        // used by the per-CPU storage implementation.
        unsafe { SOFTIRQ_STATS.remote_ref_raw(cpu) }.add_to(&mut diagnostics);
    }
    diagnostics
}

/// Clears softirq diagnostics.
#[cfg(any(unittest, feature = "irq_stat"))]
pub(crate) fn clear_softirq_diagnostics() {
    for cpu in 0..kbuild_config::NR_CPUS {
        // SAFETY: `cpu` is bounded by `NR_CPUS`, the same compile-time bound
        // used by the per-CPU storage implementation.
        unsafe { SOFTIRQ_STATS.remote_ref_raw(cpu) }.clear();
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
    let _guard = NoPreemptIrqSave::new();
    pending_ref().fetch_and(!vec.bit(), Ordering::Release);
}

#[cfg(unittest)]
#[allow(missing_docs)]
pub mod test_support {
    use super::{
        SoftirqAction, SoftirqVec, clear_softirq_diagnostics, clear_softirq_pending_for_tests,
        replace_softirq_action_for_tests, set_softirq_daemon_wake_enabled_for_tests,
    };

    pub struct ScopedSoftirqAction {
        vec: SoftirqVec,
        previous: Option<SoftirqAction>,
    }

    impl ScopedSoftirqAction {
        pub fn install(vec: SoftirqVec, action: SoftirqAction) -> Self {
            Self::replace(vec, Some(action))
        }

        pub fn clear(vec: SoftirqVec) -> Self {
            Self::replace(vec, None)
        }

        fn replace(vec: SoftirqVec, action: Option<SoftirqAction>) -> Self {
            clear_softirq_pending_for_tests(vec);
            let previous = replace_softirq_action_for_tests(vec, action);
            Self { vec, previous }
        }
    }

    impl Drop for ScopedSoftirqAction {
        fn drop(&mut self) {
            clear_softirq_pending_for_tests(self.vec);
            let _ = replace_softirq_action_for_tests(self.vec, self.previous);
        }
    }

    pub struct ScopedDaemonWakeGate {
        previous: bool,
    }

    impl ScopedDaemonWakeGate {
        pub fn disabled() -> Self {
            Self {
                previous: set_softirq_daemon_wake_enabled_for_tests(false),
            }
        }
    }

    impl Drop for ScopedDaemonWakeGate {
        fn drop(&mut self) {
            let _ = set_softirq_daemon_wake_enabled_for_tests(self.previous);
        }
    }

    pub fn begin_softirq_test() -> ScopedDaemonWakeGate {
        clear_softirq_diagnostics();
        ScopedDaemonWakeGate::disabled()
    }
}

fn run_hardirq_exit_softirqs(_ctx: DeferredRunContext) {
    let _ = run_pending_softirqs_irqoff();
}

#[inline]
fn wake_softirqd_if_needed() {
    let snapshot = context::irq_context_snapshot_irqoff();
    if snapshot.is_in_interrupt_context() {
        record_daemon_wake_suppressed_context();
        return;
    }
    wake_softirqd();
}

#[inline]
fn mark_softirq_pending(vec: SoftirqVec) -> bool {
    let previous = pending_ref().fetch_or(vec.bit(), Ordering::Release);
    previous & SOFTIRQ_VALID_MASK == 0
}

#[inline]
fn wake_softirqd() {
    record_daemon_wake_request();

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

fn run_pending_batch(mut pending: usize) {
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
            record_unhandled_vector_warning();
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
                let warning_count = record_context_leak_warning();
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
        record_softirq_run();
    }

    karch::disable_local_irq();
}

#[inline]
fn pending_ref() -> &'static AtomicUsize {
    // SAFETY: callers pin the current CPU before accessing this per-CPU atomic
    // slot. The atomic itself handles IRQ re-entry publication.
    unsafe { SOFTIRQ_PENDING.current_ref_raw() }
}

#[cfg(any(unittest, feature = "irq_stat"))]
#[inline]
fn softirq_stats_current() -> &'static SoftirqCpuStats {
    // SAFETY: callers are already running on a valid CPU; the returned slot is
    // per-CPU and all fields are atomic for interrupt re-entry publication.
    unsafe { SOFTIRQ_STATS.current_ref_raw() }
}

#[cfg(any(unittest, feature = "irq_stat"))]
#[inline]
fn softirq_stat_inc(counter: &AtomicUsize) -> usize {
    counter.fetch_add(1, Ordering::Relaxed) + 1
}

#[inline]
fn record_softirq_run() {
    #[cfg(any(unittest, feature = "irq_stat"))]
    softirq_stat_inc(&softirq_stats_current().runs);
}

#[inline]
fn record_restart_limit_hit() {
    #[cfg(any(unittest, feature = "irq_stat"))]
    softirq_stat_inc(&softirq_stats_current().restart_limit_hits);
}

#[inline]
fn record_unhandled_vector_warning() {
    #[cfg(any(unittest, feature = "irq_stat"))]
    softirq_stat_inc(&softirq_stats_current().unhandled_vector_warnings);
}

#[inline]
fn record_daemon_wake_request() {
    #[cfg(any(unittest, feature = "irq_stat"))]
    softirq_stat_inc(&softirq_stats_current().daemon_wake_requests);
}

#[inline]
fn record_daemon_wake_suppressed_context() {
    #[cfg(any(unittest, feature = "irq_stat"))]
    softirq_stat_inc(&softirq_stats_current().daemon_wake_suppressed_context);
}

#[inline]
fn record_context_deferred_run() {
    #[cfg(any(unittest, feature = "irq_stat"))]
    softirq_stat_inc(&softirq_stats_current().context_deferred_runs);
}

#[cfg(any(unittest, feature = "irq_context_debug"))]
#[inline]
fn record_context_leak_warning() -> usize {
    #[cfg(any(unittest, feature = "irq_stat"))]
    {
        softirq_stat_inc(&softirq_stats_current().context_leak_warnings)
    }

    #[cfg(not(any(unittest, feature = "irq_stat")))]
    {
        1
    }
}

#[cfg(any(unittest, feature = "irq_context_debug"))]
#[inline]
fn record_irqoff_misuse_warning() -> usize {
    #[cfg(any(unittest, feature = "irq_stat"))]
    {
        softirq_stat_inc(&softirq_stats_current().irqoff_misuse_warnings)
    }

    #[cfg(not(any(unittest, feature = "irq_stat")))]
    {
        1
    }
}

#[cfg(unittest)]
#[allow(missing_docs)]
pub mod tests_softirq {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use unittest::{assert, assert_eq, def_test};

    use super::{
        SoftirqDiagnostics, SoftirqRunResult, SoftirqVec, clear_softirq_diagnostics,
        local_softirq_pending, raise_softirq, raise_softirq_irqoff, run_pending_softirqs,
        softirq_diagnostics,
        test_support::{ScopedSoftirqAction, begin_softirq_test},
    };
    use crate::context::{
        HardIrqContextGuard, SoftIrqContextGuard, clear_irq_context_diagnostics,
        irq_context_diagnostics, irq_context_snapshot, local_bh_disable,
    };

    static ORDER_SEQ: AtomicUsize = AtomicUsize::new(1);
    static HIGH_ORDER: AtomicUsize = AtomicUsize::new(0);
    static TIMER_ORDER: AtomicUsize = AtomicUsize::new(0);
    static RERAISE_CALLS: AtomicUsize = AtomicUsize::new(0);
    static LIMIT_CALLS: AtomicUsize = AtomicUsize::new(0);
    static ACTION_IRQ_ENABLED: AtomicUsize = AtomicUsize::new(0);

    fn record_order(slot: &AtomicUsize) {
        if slot.load(Ordering::Relaxed) == 0 {
            let order = ORDER_SEQ.fetch_add(1, Ordering::Relaxed);
            let _ = slot.compare_exchange(0, order, Ordering::Relaxed, Ordering::Relaxed);
        }
    }

    fn high_action() {
        record_order(&HIGH_ORDER);
    }

    fn timer_action() {
        record_order(&TIMER_ORDER);
    }

    fn record_local_irq_state_action() {
        ACTION_IRQ_ENABLED.store(karch::local_irq_enabled() as usize, Ordering::Relaxed);
    }

    fn reraise_action() {
        let calls = RERAISE_CALLS.fetch_add(1, Ordering::Relaxed);
        if calls == 0 {
            raise_softirq(SoftirqVec::Rcu);
        }
    }

    fn always_reraise_action() {
        LIMIT_CALLS.fetch_add(1, Ordering::Relaxed);
        raise_softirq(SoftirqVec::Sched);
    }

    fn leak_softirq_context_action() {
        let guard = SoftIrqContextGuard::enter();
        core::mem::forget(guard);
    }

    #[def_test(serial)]
    fn test_softirq_runs_in_vector_order() {
        let _wake_gate = begin_softirq_test();
        ORDER_SEQ.store(1, Ordering::Relaxed);
        HIGH_ORDER.store(0, Ordering::Relaxed);
        TIMER_ORDER.store(0, Ordering::Relaxed);
        let _high = ScopedSoftirqAction::install(SoftirqVec::High, high_action);
        let _timer = ScopedSoftirqAction::install(SoftirqVec::Timer, timer_action);

        raise_softirq(SoftirqVec::Timer);
        raise_softirq(SoftirqVec::High);

        assert_eq!(run_pending_softirqs(), SoftirqRunResult::Ran);
        assert!(HIGH_ORDER.load(Ordering::Relaxed) < TIMER_ORDER.load(Ordering::Relaxed));
    }

    #[def_test(serial)]
    fn test_local_bh_guard_defers_until_outer_drop() {
        let _wake_gate = begin_softirq_test();
        HIGH_ORDER.store(0, Ordering::Relaxed);
        let _high = ScopedSoftirqAction::install(SoftirqVec::High, high_action);

        {
            let _outer = local_bh_disable();
            {
                let _inner = local_bh_disable();
                raise_softirq(SoftirqVec::High);
                assert_eq!(HIGH_ORDER.load(Ordering::Relaxed), 0);
            }
            assert_eq!(HIGH_ORDER.load(Ordering::Relaxed), 0);
        }

        assert!(HIGH_ORDER.load(Ordering::Relaxed) != 0);
        assert_eq!(local_softirq_pending(), 0);
    }

    #[def_test(serial)]
    fn test_task_context_raise_requests_softirq_daemon_wake() {
        let _wake_gate = begin_softirq_test();
        let _timer = ScopedSoftirqAction::clear(SoftirqVec::Timer);

        raise_softirq(SoftirqVec::Timer);

        let diagnostics = softirq_diagnostics();
        assert_eq!(diagnostics.daemon_wake_requests, 1);
        assert_eq!(diagnostics.daemon_wake_suppressed_context, 0);
    }

    #[def_test(serial)]
    fn test_softirq_action_runs_with_local_irq_enabled() {
        let _wake_gate = begin_softirq_test();
        ACTION_IRQ_ENABLED.store(0, Ordering::Relaxed);
        let _block = ScopedSoftirqAction::install(SoftirqVec::Block, record_local_irq_state_action);

        raise_softirq(SoftirqVec::Block);

        assert_eq!(run_pending_softirqs(), SoftirqRunResult::Ran);
        assert_eq!(ACTION_IRQ_ENABLED.load(Ordering::Relaxed), 1);
        assert_eq!(local_softirq_pending(), 0);
    }

    #[def_test(serial)]
    fn test_softirq_reraise_is_not_lost() {
        let _wake_gate = begin_softirq_test();
        RERAISE_CALLS.store(0, Ordering::Relaxed);
        let _rcu = ScopedSoftirqAction::install(SoftirqVec::Rcu, reraise_action);

        raise_softirq(SoftirqVec::Rcu);
        assert_eq!(run_pending_softirqs(), SoftirqRunResult::Ran);
        assert_eq!(RERAISE_CALLS.load(Ordering::Relaxed), 2);
    }

    #[def_test(serial)]
    fn test_softirq_restart_limit_preserves_pending() {
        let _wake_gate = begin_softirq_test();
        LIMIT_CALLS.store(0, Ordering::Relaxed);
        let _sched = ScopedSoftirqAction::install(SoftirqVec::Sched, always_reraise_action);

        raise_softirq(SoftirqVec::Sched);
        clear_softirq_diagnostics();

        assert_eq!(run_pending_softirqs(), SoftirqRunResult::Deferred);
        assert!(LIMIT_CALLS.load(Ordering::Relaxed) >= 10);
        assert!(local_softirq_pending() & (1usize << SoftirqVec::Sched.as_usize()) != 0);
        assert_eq!(softirq_diagnostics().restart_limit_hits, 1);
        assert_eq!(softirq_diagnostics().daemon_wake_requests, 1);
    }

    #[def_test(serial)]
    fn test_softirq_deferred_in_bh_disabled_context() {
        let _wake_gate = begin_softirq_test();
        HIGH_ORDER.store(0, Ordering::Relaxed);
        let _high = ScopedSoftirqAction::install(SoftirqVec::High, high_action);
        let guard = local_bh_disable();
        raise_softirq(SoftirqVec::High);

        assert_eq!(run_pending_softirqs(), SoftirqRunResult::Deferred);
        assert_eq!(softirq_diagnostics().context_deferred_runs, 1);
        assert_eq!(softirq_diagnostics().daemon_wake_requests, 0);
        assert_eq!(softirq_diagnostics().daemon_wake_suppressed_context, 1);
        assert_eq!(HIGH_ORDER.load(Ordering::Relaxed), 0);
        drop(guard);
        assert!(HIGH_ORDER.load(Ordering::Relaxed) != 0);
    }

    #[def_test(serial)]
    fn test_softirq_context_restored_after_unhandled_vector() {
        let _wake_gate = begin_softirq_test();
        let _irq_poll = ScopedSoftirqAction::clear(SoftirqVec::IrqPoll);
        let before = irq_context_snapshot();
        raise_softirq(SoftirqVec::IrqPoll);
        let _ = run_pending_softirqs();
        assert_eq!(irq_context_snapshot(), before);
        let diagnostics = softirq_diagnostics();
        assert!(diagnostics.unhandled_vector_warnings >= 1);
    }

    #[def_test(serial)]
    fn test_softirq_context_restored_after_handler_leak() {
        let _wake_gate = begin_softirq_test();
        let _tasklet =
            ScopedSoftirqAction::install(SoftirqVec::Tasklet, leak_softirq_context_action);
        let before = irq_context_snapshot();

        raise_softirq(SoftirqVec::Tasklet);
        assert_eq!(run_pending_softirqs(), SoftirqRunResult::Ran);

        assert_eq!(irq_context_snapshot(), before);
        assert_eq!(softirq_diagnostics().context_leak_warnings, 1);
    }

    #[def_test(serial)]
    fn test_softirq_deferred_in_hardirq_context() {
        let _wake_gate = begin_softirq_test();
        clear_irq_context_diagnostics();
        HIGH_ORDER.store(0, Ordering::Relaxed);
        let _high = ScopedSoftirqAction::install(SoftirqVec::High, high_action);

        {
            let _hardirq = HardIrqContextGuard::enter();
            let bh_guard = local_bh_disable();
            raise_softirq(SoftirqVec::High);

            assert_eq!(run_pending_softirqs(), SoftirqRunResult::Deferred);
            drop(bh_guard);
            assert_eq!(HIGH_ORDER.load(Ordering::Relaxed), 0);
            assert_eq!(irq_context_diagnostics().bh_in_hardirq_warnings, 1);
            assert_eq!(softirq_diagnostics().context_deferred_runs, 1);
            assert_eq!(softirq_diagnostics().daemon_wake_suppressed_context, 1);
        }

        assert_eq!(run_pending_softirqs(), SoftirqRunResult::Ran);
        assert!(HIGH_ORDER.load(Ordering::Relaxed) != 0);
    }

    #[def_test(serial)]
    fn test_repeated_task_context_raise_requests_single_daemon_wake() {
        let _wake_gate = begin_softirq_test();
        let _timer = ScopedSoftirqAction::clear(SoftirqVec::Timer);
        let _high = ScopedSoftirqAction::clear(SoftirqVec::High);

        raise_softirq(SoftirqVec::Timer);
        raise_softirq(SoftirqVec::High);

        assert_eq!(softirq_diagnostics().daemon_wake_requests, 1);
        assert_eq!(softirq_diagnostics().daemon_wake_suppressed_context, 0);
    }

    #[def_test(serial)]
    fn test_raise_softirq_irqoff_reports_enabled_irq_misuse() {
        let _wake_gate = begin_softirq_test();
        let _timer = ScopedSoftirqAction::clear(SoftirqVec::Timer);

        assert!(karch::local_irq_enabled());

        raise_softirq_irqoff(SoftirqVec::Timer);

        assert_eq!(softirq_diagnostics().irqoff_misuse_warnings, 1);
        assert!(karch::local_irq_enabled());
        assert!(local_softirq_pending() & (1usize << SoftirqVec::Timer.as_usize()) != 0);
    }

    #[def_test(serial)]
    fn test_softirq_diagnostics_clear() {
        let _wake_gate = begin_softirq_test();
        let _ = softirq_diagnostics();
        clear_softirq_diagnostics();
        assert_eq!(softirq_diagnostics(), SoftirqDiagnostics::default());
    }
}

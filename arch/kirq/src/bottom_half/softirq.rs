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
/// Actions run in IRQ-tail or BH-enable context with preemption disabled and
/// local IRQs masked. They must not sleep, allocate on the hot path, or depend
/// on process context.
pub type SoftirqAction = fn();

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

static SOFTIRQ_CONTEXT_LEAK_WARNINGS: AtomicUsize = AtomicUsize::new(0);
static SOFTIRQ_RESTART_LIMIT_HITS: AtomicUsize = AtomicUsize::new(0);
static SOFTIRQ_UNHANDLED_VECTOR_WARNINGS: AtomicUsize = AtomicUsize::new(0);
static SOFTIRQ_RUNS: AtomicUsize = AtomicUsize::new(0);

/// Softirq diagnostic counters.
#[cfg(unittest)]
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

/// Raises a softirq on the current CPU.
///
/// The function pins the current CPU and masks local IRQs while updating the
/// per-CPU pending bit.
pub fn raise_softirq(vec: SoftirqVec) {
    let _guard = NoPreemptIrqSave::new();
    pending_ref().fetch_or(vec.bit(), Ordering::Release);
}

/// Raises a softirq on the current CPU when local IRQs are already disabled.
///
/// The function still pins the current CPU before touching per-CPU state.
pub fn raise_softirq_irqoff(vec: SoftirqVec) {
    let _guard = NoPreempt::new();
    pending_ref().fetch_or(vec.bit(), Ordering::Release);
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
                SOFTIRQ_RESTART_LIMIT_HITS.fetch_add(1, Ordering::Relaxed);
                return SoftirqRunResult::Deferred;
            }
            return SoftirqRunResult::Ran;
        }
    }
}

/// Returns current softirq diagnostics.
#[cfg(unittest)]
pub(crate) fn softirq_diagnostics() -> SoftirqDiagnostics {
    SoftirqDiagnostics {
        context_leak_warnings: SOFTIRQ_CONTEXT_LEAK_WARNINGS.load(Ordering::Relaxed),
        restart_limit_hits: SOFTIRQ_RESTART_LIMIT_HITS.load(Ordering::Relaxed),
        unhandled_vector_warnings: SOFTIRQ_UNHANDLED_VECTOR_WARNINGS.load(Ordering::Relaxed),
        runs: SOFTIRQ_RUNS.load(Ordering::Relaxed),
    }
}

/// Clears softirq diagnostics.
#[cfg(unittest)]
pub(crate) fn clear_softirq_diagnostics() {
    SOFTIRQ_CONTEXT_LEAK_WARNINGS.store(0, Ordering::Relaxed);
    SOFTIRQ_RESTART_LIMIT_HITS.store(0, Ordering::Relaxed);
    SOFTIRQ_UNHANDLED_VECTOR_WARNINGS.store(0, Ordering::Relaxed);
    SOFTIRQ_RUNS.store(0, Ordering::Relaxed);
}

#[cfg(unittest)]
pub(crate) fn reset_softirq_for_tests() {
    *SOFTIRQ_ACTIONS.lock() = [None; SoftirqVec::COUNT];
    let _guard = NoPreemptIrqSave::new();
    pending_ref().store(0, Ordering::Release);
    clear_softirq_diagnostics();
}

fn run_hardirq_exit_softirqs(_ctx: DeferredRunContext) {
    let _ = run_pending_softirqs_irqoff();
}

#[inline]
fn can_run_softirqs() -> bool {
    let snapshot = context::irq_context_snapshot_irqoff();
    !snapshot.is_in_hardirq() && !snapshot.is_serving_softirq() && !snapshot.is_bh_disabled()
}

fn run_pending_batch(mut pending: usize) {
    let actions = *SOFTIRQ_ACTIONS.lock();
    while pending != 0 {
        let index = pending.trailing_zeros() as usize;
        pending &= !(1usize << index);

        let Some(vec) = SoftirqVec::from_index(index) else {
            continue;
        };
        let Some(action) = actions[vec.as_usize()] else {
            SOFTIRQ_UNHANDLED_VECTOR_WARNINGS.fetch_add(1, Ordering::Relaxed);
            warn!("softirq vector {} has no registered action", vec.as_usize());
            continue;
        };

        let before = context::irq_context_snapshot_irqoff();
        {
            let _softirq_context = SoftIrqContextGuard::enter_irqoff();
            action();
        }
        let after = context::irq_context_snapshot_irqoff();
        if after != before {
            SOFTIRQ_CONTEXT_LEAK_WARNINGS.fetch_add(1, Ordering::Relaxed);
            warn!(
                "softirq vector {} leaked context state: before={:?} after={:?}",
                vec.as_usize(),
                before,
                after
            );
            context::restore_current_state_snapshot_irqoff(before);
        }
        SOFTIRQ_RUNS.fetch_add(1, Ordering::Relaxed);
    }
}

#[inline]
fn pending_ref() -> &'static AtomicUsize {
    // SAFETY: callers pin the current CPU before accessing this per-CPU atomic
    // slot. The atomic itself handles IRQ re-entry publication.
    unsafe { SOFTIRQ_PENDING.current_ref_raw() }
}

#[cfg(unittest)]
#[allow(missing_docs)]
pub mod tests_softirq {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use unittest::{assert, assert_eq, def_test};

    use super::{
        SoftirqDiagnostics, SoftirqRunResult, SoftirqVec, clear_softirq_diagnostics,
        local_softirq_pending, open_softirq, raise_softirq, raise_softirq_irqoff,
        reset_softirq_for_tests, run_pending_softirqs, softirq_diagnostics,
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

    fn reraise_action() {
        let calls = RERAISE_CALLS.fetch_add(1, Ordering::Relaxed);
        if calls == 0 {
            raise_softirq_irqoff(SoftirqVec::Rcu);
        }
    }

    fn always_reraise_action() {
        LIMIT_CALLS.fetch_add(1, Ordering::Relaxed);
        raise_softirq_irqoff(SoftirqVec::Sched);
    }

    fn leak_softirq_context_action() {
        let guard = SoftIrqContextGuard::enter();
        core::mem::forget(guard);
    }

    #[def_test(serial)]
    fn test_softirq_runs_in_vector_order() {
        reset_softirq_for_tests();
        ORDER_SEQ.store(1, Ordering::Relaxed);
        HIGH_ORDER.store(0, Ordering::Relaxed);
        TIMER_ORDER.store(0, Ordering::Relaxed);
        let _ = open_softirq(SoftirqVec::High, high_action);
        let _ = open_softirq(SoftirqVec::Timer, timer_action);

        raise_softirq(SoftirqVec::Timer);
        raise_softirq(SoftirqVec::High);

        assert_eq!(run_pending_softirqs(), SoftirqRunResult::Ran);
        assert!(HIGH_ORDER.load(Ordering::Relaxed) < TIMER_ORDER.load(Ordering::Relaxed));
    }

    #[def_test(serial)]
    fn test_local_bh_guard_defers_until_outer_drop() {
        reset_softirq_for_tests();
        HIGH_ORDER.store(0, Ordering::Relaxed);
        let _ = open_softirq(SoftirqVec::High, high_action);

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
    fn test_softirq_reraise_is_not_lost() {
        reset_softirq_for_tests();
        RERAISE_CALLS.store(0, Ordering::Relaxed);
        let _ = open_softirq(SoftirqVec::Rcu, reraise_action);

        raise_softirq(SoftirqVec::Rcu);
        assert_eq!(run_pending_softirqs(), SoftirqRunResult::Ran);
        assert_eq!(RERAISE_CALLS.load(Ordering::Relaxed), 2);
    }

    #[def_test(serial)]
    fn test_softirq_restart_limit_preserves_pending() {
        reset_softirq_for_tests();
        LIMIT_CALLS.store(0, Ordering::Relaxed);
        let _ = open_softirq(SoftirqVec::Sched, always_reraise_action);

        raise_softirq(SoftirqVec::Sched);

        assert_eq!(run_pending_softirqs(), SoftirqRunResult::Deferred);
        assert!(LIMIT_CALLS.load(Ordering::Relaxed) >= 10);
        assert!(local_softirq_pending() & (1usize << SoftirqVec::Sched.as_usize()) != 0);
        assert_eq!(softirq_diagnostics().restart_limit_hits, 1);

        reset_softirq_for_tests();
    }

    #[def_test(serial)]
    fn test_softirq_deferred_in_bh_disabled_context() {
        reset_softirq_for_tests();
        HIGH_ORDER.store(0, Ordering::Relaxed);
        let _ = open_softirq(SoftirqVec::High, high_action);
        let guard = local_bh_disable();
        raise_softirq(SoftirqVec::High);

        assert_eq!(run_pending_softirqs(), SoftirqRunResult::Deferred);
        assert_eq!(HIGH_ORDER.load(Ordering::Relaxed), 0);
        drop(guard);
        assert!(HIGH_ORDER.load(Ordering::Relaxed) != 0);
    }

    #[def_test(serial)]
    fn test_softirq_context_restored_after_unhandled_vector() {
        reset_softirq_for_tests();
        let before = irq_context_snapshot();
        raise_softirq(SoftirqVec::IrqPoll);
        let _ = run_pending_softirqs();
        assert_eq!(irq_context_snapshot(), before);
        let diagnostics = softirq_diagnostics();
        assert!(diagnostics.unhandled_vector_warnings >= 1);
    }

    #[def_test(serial)]
    fn test_softirq_context_restored_after_handler_leak() {
        reset_softirq_for_tests();
        let _ = open_softirq(SoftirqVec::Tasklet, leak_softirq_context_action);
        let before = irq_context_snapshot();

        raise_softirq(SoftirqVec::Tasklet);
        assert_eq!(run_pending_softirqs(), SoftirqRunResult::Ran);

        assert_eq!(irq_context_snapshot(), before);
        assert_eq!(softirq_diagnostics().context_leak_warnings, 1);
    }

    #[def_test(serial)]
    fn test_softirq_deferred_in_hardirq_context() {
        reset_softirq_for_tests();
        clear_irq_context_diagnostics();
        HIGH_ORDER.store(0, Ordering::Relaxed);
        let _ = open_softirq(SoftirqVec::High, high_action);

        {
            let _hardirq = HardIrqContextGuard::enter();
            let bh_guard = local_bh_disable();
            raise_softirq(SoftirqVec::High);

            assert_eq!(run_pending_softirqs(), SoftirqRunResult::Deferred);
            drop(bh_guard);
            assert_eq!(HIGH_ORDER.load(Ordering::Relaxed), 0);
            assert_eq!(irq_context_diagnostics().bh_in_hardirq_warnings, 1);
        }

        assert_eq!(run_pending_softirqs(), SoftirqRunResult::Ran);
        assert!(HIGH_ORDER.load(Ordering::Relaxed) != 0);
    }

    #[def_test(serial)]
    fn test_softirq_diagnostics_clear() {
        reset_softirq_for_tests();
        let _ = softirq_diagnostics();
        clear_softirq_diagnostics();
        assert_eq!(softirq_diagnostics(), SoftirqDiagnostics::default());
    }
}

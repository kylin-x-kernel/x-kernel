// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IRQ execution-context tracking and diagnostics.
//!
//! This module tracks the generic IRQ-core context visible to bottom-half
//! infrastructure. It is intentionally separate from scheduler preemption
//! accounting: `kspin` guards still control task preemption, while this module
//! answers whether the current CPU is inside a hardirq handler, serving softirq
//! work, or running with local bottom halves disabled.
//!
//! Hardirq, softirq, and BH-disabled contexts are all non-sleepable. Future
//! IRQ-thread execution will need its own sleepable context model instead of
//! being represented by [`is_in_interrupt_context`].

use core::marker::PhantomData;
#[cfg(any(unittest, feature = "irq_stat"))]
use core::sync::atomic::{AtomicUsize, Ordering};

use kspin::{NoPreempt, NoPreemptIrqSave};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct IrqContextState {
    hardirq_depth: usize,
    softirq_depth: usize,
    bh_disable_depth: usize,
}

impl IrqContextState {
    const fn empty() -> Self {
        Self {
            hardirq_depth: 0,
            softirq_depth: 0,
            bh_disable_depth: 0,
        }
    }

    const fn snapshot(self) -> IrqContextSnapshot {
        IrqContextSnapshot {
            hardirq_depth: self.hardirq_depth,
            softirq_depth: self.softirq_depth,
            bh_disable_depth: self.bh_disable_depth,
        }
    }
}

#[percpu::def_percpu]
static IRQ_CONTEXT_STATE: IrqContextState = IrqContextState::empty();

#[cfg(any(unittest, feature = "irq_stat"))]
static CONTEXT_UNDERFLOW_WARNINGS: AtomicUsize = AtomicUsize::new(0);
#[cfg(any(unittest, feature = "irq_stat"))]
static BH_IN_HARDIRQ_WARNINGS: AtomicUsize = AtomicUsize::new(0);

#[cfg(any(unittest, feature = "irq_context_debug"))]
const INITIAL_CONTEXT_WARNING_LOGS: usize = 8;

/// Snapshot of the current CPU's IRQ execution-context counters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IrqContextSnapshot {
    hardirq_depth: usize,
    softirq_depth: usize,
    bh_disable_depth: usize,
}

impl IrqContextSnapshot {
    /// Returns the hardirq nesting depth recorded for the current CPU.
    #[inline]
    pub const fn hardirq_depth(&self) -> usize {
        self.hardirq_depth
    }

    /// Returns the softirq handler nesting depth recorded for the current CPU.
    #[inline]
    pub const fn softirq_depth(&self) -> usize {
        self.softirq_depth
    }

    /// Returns the local bottom-half disable depth recorded for the current CPU.
    #[inline]
    pub const fn bh_disable_depth(&self) -> usize {
        self.bh_disable_depth
    }

    /// Returns whether this snapshot represents hardirq context.
    #[inline]
    pub const fn is_in_hardirq(&self) -> bool {
        self.hardirq_depth != 0
    }

    /// Returns whether this snapshot represents active softirq handler context.
    #[inline]
    pub const fn is_serving_softirq(&self) -> bool {
        self.softirq_depth != 0
    }

    /// Returns whether local bottom halves are disabled in this snapshot.
    #[inline]
    pub const fn is_bh_disabled(&self) -> bool {
        self.bh_disable_depth != 0
    }

    /// Returns whether this snapshot is interrupt-like for bottom-half gating.
    ///
    /// This is a non-sleepable predicate for hardirq, active softirq handling,
    /// and BH-disabled task context. It must not be used as a future IRQ-thread
    /// predicate because IRQ threads are sleepable execution contexts.
    #[inline]
    pub const fn is_in_interrupt_context(&self) -> bool {
        self.is_in_hardirq() || self.is_serving_softirq() || self.is_bh_disabled()
    }
}

/// Coarse interrupt context level for diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InterruptContextLevel {
    /// Ordinary task context.
    Task,
    /// Ordinary task context with local bottom halves disabled.
    BhDisabled,
    /// Currently serving softirq work.
    Softirq,
    /// Currently serving a normal hardirq.
    Hardirq,
}

/// IRQ-context diagnostic counters.
#[cfg(any(unittest, feature = "irq_stat"))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct IrqContextDiagnostics {
    /// Number of detected context counter underflow attempts.
    pub context_underflow_warnings: usize,
    /// Number of bottom-half disable calls observed from hardirq context.
    pub bh_in_hardirq_warnings: usize,
}

/// Returns a snapshot of the current CPU's IRQ execution context.
///
/// The function pins the current CPU and masks local IRQs while reading the
/// per-CPU state so callers do not need to establish their own guard.
#[inline]
pub fn irq_context_snapshot() -> IrqContextSnapshot {
    let _guard = NoPreemptIrqSave::new();
    current_state_snapshot()
}

/// Returns the current CPU's coarse interrupt context level.
#[inline]
pub fn interrupt_context_level() -> InterruptContextLevel {
    let snapshot = irq_context_snapshot();
    if snapshot.is_in_hardirq() {
        InterruptContextLevel::Hardirq
    } else if snapshot.is_serving_softirq() {
        InterruptContextLevel::Softirq
    } else if snapshot.is_bh_disabled() {
        InterruptContextLevel::BhDisabled
    } else {
        InterruptContextLevel::Task
    }
}

/// Returns whether the current CPU is inside a normal hardirq handler.
#[inline]
pub fn is_in_hardirq() -> bool {
    irq_context_snapshot().is_in_hardirq()
}

/// Returns whether the current CPU is running a softirq handler.
#[inline]
pub fn is_serving_softirq() -> bool {
    irq_context_snapshot().is_serving_softirq()
}

/// Returns whether local bottom halves are disabled on the current CPU.
#[inline]
pub fn is_bh_disabled() -> bool {
    irq_context_snapshot().is_bh_disabled()
}

/// Returns whether the current CPU is in hardirq, softirq, or BH-disabled state.
///
/// Prefer the more specific query helpers in new code. This combined predicate
/// is intended for bottom-half gating and diagnostics. A future sleepable IRQ
/// thread must not be reported through this predicate.
#[inline]
pub fn is_in_interrupt_context() -> bool {
    irq_context_snapshot().is_in_interrupt_context()
}

/// Returns current IRQ-context diagnostic counters.
#[cfg(any(unittest, feature = "irq_stat"))]
#[inline]
pub(crate) fn irq_context_diagnostics() -> IrqContextDiagnostics {
    IrqContextDiagnostics {
        context_underflow_warnings: CONTEXT_UNDERFLOW_WARNINGS.load(Ordering::Relaxed),
        bh_in_hardirq_warnings: BH_IN_HARDIRQ_WARNINGS.load(Ordering::Relaxed),
    }
}

/// Clears IRQ-context diagnostic counters.
///
/// This is primarily intended for focused tests and controlled diagnostics.
#[cfg(any(unittest, feature = "irq_stat"))]
#[inline]
pub(crate) fn clear_irq_context_diagnostics() {
    CONTEXT_UNDERFLOW_WARNINGS.store(0, Ordering::Relaxed);
    BH_IN_HARDIRQ_WARNINGS.store(0, Ordering::Relaxed);
}

/// Disables local bottom halves until the returned guard is dropped.
///
/// The guard keeps preemption disabled for its full lifetime, so its enter,
/// drop, and possible outermost softirq drain all operate on the same CPU.
/// Local IRQs are only masked while the per-CPU depth is being updated.
#[inline]
pub fn local_bh_disable() -> LocalBhGuard {
    LocalBhGuard::new()
}

pub(crate) struct HardIrqContextGuard;

impl HardIrqContextGuard {
    /// Enters hardirq context when the caller already masked local IRQs and
    /// pinned the current CPU.
    pub(crate) fn enter() -> Self {
        with_current_state_irqoff(|state| {
            state.hardirq_depth = state.hardirq_depth.saturating_add(1);
        });
        Self
    }
}

impl Drop for HardIrqContextGuard {
    fn drop(&mut self) {
        with_current_state_irqoff(|state| {
            if state.hardirq_depth == 0 {
                warn_context_underflow("hardirq");
            } else {
                state.hardirq_depth -= 1;
            }
        });
    }
}

#[cfg(unittest)]
pub mod test_support {
    use super::HardIrqContextGuard;

    /// Test-only guard that enters hardirq context until dropped.
    pub struct ScopedHardIrqContext {
        _guard: HardIrqContextGuard,
    }

    impl ScopedHardIrqContext {
        /// Enters a synthetic hardirq context for cross-crate unit tests.
        pub fn enter() -> Self {
            Self {
                _guard: HardIrqContextGuard::enter(),
            }
        }
    }
}

pub(crate) struct SoftIrqContextGuard {
    irqoff: bool,
}

impl SoftIrqContextGuard {
    #[cfg(unittest)]
    pub(crate) fn enter() -> Self {
        with_current_state(|state| {
            state.softirq_depth = state.softirq_depth.saturating_add(1);
        });
        Self { irqoff: false }
    }

    pub(crate) fn enter_irqoff() -> Self {
        with_current_state_irqoff(|state| {
            state.softirq_depth = state.softirq_depth.saturating_add(1);
        });
        Self { irqoff: true }
    }
}

impl Drop for SoftIrqContextGuard {
    fn drop(&mut self) {
        let exit = |state: &mut IrqContextState| {
            if state.softirq_depth == 0 {
                warn_context_underflow("softirq");
            } else {
                state.softirq_depth -= 1;
            }
        };
        if self.irqoff {
            with_current_state_irqoff(exit);
        } else {
            with_current_state(exit);
        }
    }
}

/// RAII guard returned by [`local_bh_disable`].
pub struct LocalBhGuard {
    _no_preempt: NoPreempt,
    _not_send: PhantomData<*mut ()>,
}

impl LocalBhGuard {
    fn new() -> Self {
        let no_preempt = NoPreempt::new();
        with_current_state(|state| {
            #[cfg(any(unittest, feature = "irq_context_debug"))]
            if state.hardirq_depth != 0 {
                let warning_count = record_bh_in_hardirq_warning();
                if should_log_context_warning(warning_count) {
                    warn!(
                        "local_bh_disable called from hardirq context ({warning_count} warnings)"
                    );
                }
            }
            state.bh_disable_depth = state.bh_disable_depth.saturating_add(1);
        });
        Self {
            _no_preempt: no_preempt,
            _not_send: PhantomData,
        }
    }
}

impl Drop for LocalBhGuard {
    fn drop(&mut self) {
        let should_run_softirqs = with_current_state_result(|state| {
            if state.bh_disable_depth == 0 {
                warn_context_underflow("bh");
                return false;
            }

            state.bh_disable_depth -= 1;
            state.bh_disable_depth == 0 && state.hardirq_depth == 0 && state.softirq_depth == 0
        });

        if should_run_softirqs {
            crate::softirq::run_pending_softirqs();
        }
    }
}

#[inline]
fn current_state_snapshot() -> IrqContextSnapshot {
    // SAFETY: the caller pins the current CPU and masks local IRQs before
    // reading this non-atomic per-CPU slot.
    unsafe { (*IRQ_CONTEXT_STATE.current_ref_raw()).snapshot() }
}

/// Returns the current CPU IRQ-context snapshot when the caller already masked
/// local IRQs and pinned the current CPU.
#[inline]
pub(crate) fn irq_context_snapshot_irqoff() -> IrqContextSnapshot {
    current_state_snapshot()
}

#[inline]
fn with_current_state(f: impl FnOnce(&mut IrqContextState)) {
    with_current_state_result(|state| {
        f(state);
    });
}

#[inline]
fn with_current_state_result<T>(f: impl FnOnce(&mut IrqContextState) -> T) -> T {
    let _guard = NoPreemptIrqSave::new();
    with_current_state_irqoff(f)
}

#[inline]
fn with_current_state_irqoff<T>(f: impl FnOnce(&mut IrqContextState) -> T) -> T {
    // SAFETY: the caller pins the current CPU and masks local IRQs while the
    // current per-CPU slot is mutably borrowed.
    let state = unsafe { IRQ_CONTEXT_STATE.current_ref_mut_raw() };
    f(state)
}

/// Restores the current CPU IRQ-context snapshot when the caller already masked
/// local IRQs and pinned the current CPU.
#[cfg(any(unittest, feature = "irq_context_debug"))]
pub(crate) fn restore_current_state_snapshot_irqoff(snapshot: IrqContextSnapshot) {
    with_current_state_irqoff(|state| {
        *state = IrqContextState {
            hardirq_depth: snapshot.hardirq_depth,
            softirq_depth: snapshot.softirq_depth,
            bh_disable_depth: snapshot.bh_disable_depth,
        };
    });
}

fn warn_context_underflow(context: &'static str) {
    #[cfg(any(unittest, feature = "irq_context_debug"))]
    {
        let warning_count = record_context_underflow_warning();
        if should_log_context_warning(warning_count) {
            warn!("IRQ context {context} depth underflow ({warning_count} warnings)");
        }
    }

    #[cfg(not(any(unittest, feature = "irq_context_debug")))]
    let _ = context;
}

#[cfg(any(unittest, feature = "irq_context_debug"))]
#[inline]
fn record_context_underflow_warning() -> usize {
    #[cfg(any(unittest, feature = "irq_stat"))]
    {
        CONTEXT_UNDERFLOW_WARNINGS.fetch_add(1, Ordering::Relaxed) + 1
    }

    #[cfg(not(any(unittest, feature = "irq_stat")))]
    {
        1
    }
}

#[cfg(any(unittest, feature = "irq_context_debug"))]
#[inline]
fn record_bh_in_hardirq_warning() -> usize {
    #[cfg(any(unittest, feature = "irq_stat"))]
    {
        BH_IN_HARDIRQ_WARNINGS.fetch_add(1, Ordering::Relaxed) + 1
    }

    #[cfg(not(any(unittest, feature = "irq_stat")))]
    {
        1
    }
}

#[cfg(any(unittest, feature = "irq_context_debug"))]
#[inline]
fn should_log_context_warning(warning_count: usize) -> bool {
    warning_count <= INITIAL_CONTEXT_WARNING_LOGS || warning_count.is_power_of_two()
}

#[cfg(unittest)]
#[allow(missing_docs)]
pub mod tests_context {
    use unittest::{assert, assert_eq, def_test};

    use super::{
        HardIrqContextGuard, InterruptContextLevel, SoftIrqContextGuard,
        clear_irq_context_diagnostics, interrupt_context_level, irq_context_diagnostics,
        irq_context_snapshot, is_bh_disabled, is_in_hardirq, is_in_interrupt_context,
        is_serving_softirq, local_bh_disable,
    };

    #[def_test(serial)]
    fn test_irq_context_snapshot_empty_by_default() {
        clear_irq_context_diagnostics();
        let snapshot = irq_context_snapshot();
        assert_eq!(snapshot.hardirq_depth(), 0);
        assert_eq!(snapshot.softirq_depth(), 0);
        assert_eq!(snapshot.bh_disable_depth(), 0);
        assert_eq!(interrupt_context_level(), InterruptContextLevel::Task);
        assert!(!is_in_interrupt_context());
    }

    #[def_test(serial)]
    fn test_hardirq_context_guard_sets_and_restores_depth() {
        clear_irq_context_diagnostics();
        let _irq_guard = kspin::NoPreemptIrqSave::new();
        {
            let _guard = HardIrqContextGuard::enter();
            assert!(is_in_hardirq());
            assert!(is_in_interrupt_context());
            assert_eq!(interrupt_context_level(), InterruptContextLevel::Hardirq);
            assert_eq!(irq_context_snapshot().hardirq_depth(), 1);
        }
        assert!(!is_in_hardirq());
        assert!(!is_in_interrupt_context());
        assert_eq!(irq_context_diagnostics().context_underflow_warnings, 0);
    }

    #[def_test(serial)]
    fn test_hardirq_context_guard_supports_nesting() {
        clear_irq_context_diagnostics();
        let _irq_guard = kspin::NoPreemptIrqSave::new();
        {
            let _outer = HardIrqContextGuard::enter();
            {
                let _inner = HardIrqContextGuard::enter();
                assert_eq!(irq_context_snapshot().hardirq_depth(), 2);
            }
            assert_eq!(irq_context_snapshot().hardirq_depth(), 1);
        }
        assert_eq!(irq_context_snapshot().hardirq_depth(), 0);
    }

    #[def_test(serial)]
    fn test_softirq_context_guard_sets_serving_state() {
        clear_irq_context_diagnostics();
        {
            let _guard = SoftIrqContextGuard::enter();
            assert!(is_serving_softirq());
            assert!(is_in_interrupt_context());
            assert_eq!(interrupt_context_level(), InterruptContextLevel::Softirq);
        }
        assert!(!is_serving_softirq());
    }

    #[def_test(serial)]
    fn test_hardirq_level_takes_precedence_over_softirq() {
        clear_irq_context_diagnostics();
        let _softirq = SoftIrqContextGuard::enter();
        let _irq_guard = kspin::NoPreemptIrqSave::new();
        let _hardirq = HardIrqContextGuard::enter();
        assert_eq!(interrupt_context_level(), InterruptContextLevel::Hardirq);
    }

    #[def_test(serial)]
    fn test_local_bh_guard_sets_and_restores_depth() {
        clear_irq_context_diagnostics();
        {
            let _guard = local_bh_disable();
            assert!(is_bh_disabled());
            assert!(is_in_interrupt_context());
            assert_eq!(interrupt_context_level(), InterruptContextLevel::BhDisabled);
            assert_eq!(irq_context_snapshot().bh_disable_depth(), 1);
        }
        assert!(!is_bh_disabled());
        assert!(!is_in_interrupt_context());
    }

    #[def_test(serial)]
    fn test_softirq_level_takes_precedence_over_bh_disabled() {
        clear_irq_context_diagnostics();
        let _bh = local_bh_disable();
        let _softirq = SoftIrqContextGuard::enter();
        assert_eq!(interrupt_context_level(), InterruptContextLevel::Softirq);
    }

    #[def_test(serial)]
    fn test_nested_local_bh_guard_restores_outer_depth() {
        clear_irq_context_diagnostics();
        {
            let _outer = local_bh_disable();
            {
                let _inner = local_bh_disable();
                assert_eq!(irq_context_snapshot().bh_disable_depth(), 2);
            }
            assert_eq!(irq_context_snapshot().bh_disable_depth(), 1);
        }
        assert_eq!(irq_context_snapshot().bh_disable_depth(), 0);
    }
}

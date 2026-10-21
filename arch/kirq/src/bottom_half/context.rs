// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IRQ execution-context tracking and diagnostics.
//!
//! This module tracks the generic IRQ-core context visible to bottom-half
//! infrastructure. It is intentionally separate from scheduler preemption
//! accounting: `kspin` guards still control task preemption, while this module
//! answers whether the current CPU is inside a hardirq handler, serving softirq
//! work, or running with local bottom halves disabled. Effective NMI scopes
//! have a separate lock-free counter so coarse queries can bypass those locks.
//!
//! Hardirq, softirq, and BH-disabled contexts are all non-sleepable. Future
//! IRQ-thread execution will need its own sleepable context model instead of
//! being represented by [`is_in_interrupt_context`].

use core::{
    marker::PhantomData,
    sync::atomic::{AtomicUsize, Ordering},
};

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

// Macro-generated wrapper modules do not inherit the invocation site's
// lexical scope and interpolated `super` resolves against the invocation
// module's parent, so private local types must be anchored absolutely.
kcpu_slot::cpu_slot_cell_with_template! {
    static IRQ_CONTEXT_STATE: kspin::SpinNoIrq<crate::bottom_half::context::IrqContextState> =
        unsafe { kspin::SpinNoIrq::new(crate::bottom_half::context::IrqContextState::empty()) };
}

// Unlike ordinary context counters, NMI entry must not take an interrupted
// CPU's lock. These count effective scopes, not physical interrupt arrivals:
// a dedicated NMI route can contain a nested controller-classified scope.
kcpu_slot::cpu_slot! {
    static NMI_SCOPE_DEPTH: core::sync::atomic::AtomicUsize =
        core::sync::atomic::AtomicUsize::new(0);
}

pub(crate) struct NmiContextGuard {
    depth: &'static AtomicUsize,
    cpu_local: PhantomData<*mut ()>,
}

impl NmiContextGuard {
    /// # Safety
    /// CPU slots must be initialized. Remain on this CPU with ordinary IRQs
    /// excluded until this guard is dropped; nested entries must unwind first.
    pub(crate) unsafe fn enter() -> Self {
        let depth = NMI_SCOPE_DEPTH.current_shared();
        // An address-sized count cannot overflow within a kernel stack's
        // bounded nesting. RMW retains nested updates without exclusive borrows;
        // acquire keeps the scope's work after effective NMI entry.
        depth.fetch_add(1, Ordering::AcqRel);
        Self {
            depth,
            cpu_local: PhantomData,
        }
    }
}

impl Drop for NmiContextGuard {
    fn drop(&mut self) {
        // Keep all scope work before removing effective NMI context.
        self.depth.fetch_sub(1, Ordering::Release);
    }
}

/// Reports effective NMI context without locks, guards or scheduler access.
///
/// Requires initialized CPU slots. The observation describes this CPU at the
/// call, not a transferable context token or a count of hardware NMI arrivals.
#[inline]
pub fn is_in_nmi() -> bool {
    NMI_SCOPE_DEPTH.current_shared().load(Ordering::Relaxed) != 0
}

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
    /// Currently in an effective NMI scope, including controller classification.
    Nmi,
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
/// This locking snapshot excludes NMI state and must not be called from NMI;
/// use [`is_in_nmi`] or [`interrupt_context_level`] there.
#[inline]
pub fn irq_context_snapshot() -> IrqContextSnapshot {
    current_state_snapshot()
}

/// Returns the current CPU's coarse interrupt context level.
#[inline]
pub fn interrupt_context_level() -> InterruptContextLevel {
    if is_in_nmi() {
        return InterruptContextLevel::Nmi;
    }
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

/// Reports effective softirq execution, excluding a nested hardirq.
///
/// Unlike [`is_serving_softirq`], this is false while a hardirq interrupts an
/// active softirq handler. It observes context without entering or pinning it.
#[inline]
pub fn is_softirq_context() -> bool {
    interrupt_context_level() == InterruptContextLevel::Softirq
}

/// Returns whether local bottom halves are disabled on the current CPU.
#[inline]
pub fn is_bh_disabled() -> bool {
    irq_context_snapshot().is_bh_disabled()
}

/// Returns whether the current CPU is in NMI, hardirq, softirq, or BH-disabled state.
///
/// Prefer the more specific query helpers in new code. This combined predicate
/// is intended for bottom-half gating and diagnostics. A future sleepable IRQ
/// thread must not be reported through this predicate.
#[inline]
pub fn is_in_interrupt_context() -> bool {
    is_in_nmi() || irq_context_snapshot().is_in_interrupt_context()
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

unittest::path_mod_test!(pub mod test_support, "../../ut-tests/bottom_half_context_test_support.rs");

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
    let _irq = kspin::IrqSave::new();
    // Queries are also used by scheduler/RCU checks. Keep the lock's guard
    // release from recursively entering those checks, then balance our credit
    // without creating a scheduling checkpoint. IRQ exclusion pins the slot.
    let preempt = NoPreempt::new();
    let snapshot = IRQ_CONTEXT_STATE.shared_current().lock().snapshot();
    preempt.release_without_check();
    snapshot
}

/// Returns the current CPU IRQ-context snapshot when the caller already masked
/// local IRQs and pinned the current CPU.
#[inline]
pub(crate) fn irq_context_snapshot_irqoff() -> IrqContextSnapshot {
    current_state_snapshot()
}

#[inline]
fn with_current_state(f: impl FnOnce(&mut IrqContextState)) {
    with_current_state_irqoff(f)
}

#[inline]
fn with_current_state_result<T>(f: impl FnOnce(&mut IrqContextState) -> T) -> T {
    with_current_state_irqoff(f)
}

#[inline]
fn with_current_state_irqoff<T>(f: impl FnOnce(&mut IrqContextState) -> T) -> T {
    // Nested save/restore is near-free on already-masked entry paths, and the
    // guard gives the mutable borrow real CPU-local exclusion instead of a
    // caller-side comment.
    let pin = NoPreemptIrqSave::new();
    // The guard-bounded accessor selects this CPU's permanently allocated lock;
    // object uniqueness is then provided by SpinNoIrq, independently of nested
    // pins.
    f(&mut IRQ_CONTEXT_STATE.get(&pin).lock())
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

unittest::path_mod_test!(pub mod tests_context, "../../ut-tests/bottom_half_context.rs");

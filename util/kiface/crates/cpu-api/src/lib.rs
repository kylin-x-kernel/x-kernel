// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! CPU identity, local IRQ masking, exception observations and reschedule transport.

#![no_std]

use kcpu_id_map::LogicalCpuId;

/// Logical CPU identity, without pinning subsequent execution.
#[kiface::interface]
pub trait CurrentCpuIf {
    /// Samples the current logical CPU without pinning subsequent operations.
    ///
    /// Never allocates, locks or checks scheduling. Callers keep their own
    /// exclusion across CPU-local queue operations that use this identity.
    fn current_cpu() -> LogicalCpuId;
}

/// Platform contract for saving and disabling local IRQs around scoped slot
/// access and the single-instruction fallback.
///
/// Architectures without a true single-instruction per-CPU access implement
/// single-instruction loads and friends by masking local IRQs, touching the
/// value, and restoring the mask. `kcpu_slot` cannot own that logic because the
/// correct mask primitive (including AArch64's PMR/NMI handling) lives above it
/// in `karch`; the platform thus provides exactly one implementation.
#[kiface::interface]
pub trait CpuLocalIrqIf {
    /// Saves the current local IRQ state and disables local IRQs, returning an
    /// opaque token for [`Self::restore`].
    fn save_and_disable() -> usize;
    /// Restores the local IRQ state previously returned by
    /// [`Self::save_and_disable`].
    fn restore(state: usize);
}

/// Protected observations of CPU exception context.
#[kiface::interface]
pub trait ExceptionContextIf {
    /// Samples exception presence under temporary IRQ exclusion, then restores it.
    ///
    /// Do not dereference the trapframe or run a scheduling/cleanup check.
    fn is_in_exception_context() -> bool;

    /// Lends exception presence to one synchronous body while ordinary IRQs stay masked.
    ///
    /// Pin before sampling the per-CPU exception marker and retain that same
    /// exclusion through the body. Restore the caller's mask on return or unwind,
    /// without changing preemption counts or running scheduling/cleanup checks.
    /// Do not allocate, lock or access current just to supply the snapshot.
    /// The body must not enable IRQs, block or migrate; it owns admission checks.
    fn with_exception_snapshot(body_fn: &mut dyn FnMut(bool));
}

/// Cross-CPU scheduling notification, independent of the requesting subsystem.
#[kiface::interface]
pub trait CpuRescheduleNotifyIf {
    /// Requests a scheduling opportunity without reporting quiescence itself.
    ///
    /// Requires initialized IPI delivery. Must not allocate, wait or wake a task
    /// inline. The consumer retains the decision whether a CPU needs this notification.
    ///
    /// # Errors
    /// Returns an error when the target notification transport is unavailable.
    fn notify_reschedule(cpu: LogicalCpuId) -> kerrno::KResult<()>;
}

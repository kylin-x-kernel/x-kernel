// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

pub use kcpu_id_map::LogicalCpuId as CpuId;

/// Proof that execution is pinned to one logical CPU for the guard's lifetime.
///
/// Implemented by the platform and task layers for the guards that already
/// prevent migration: `kspin::NoPreempt` / `NoPreemptIrqSave`, IRQ and trap
/// frames, and the scheduler's pinned context. Passing one to a slot accessor
/// is the only guarantee those accessors rely on; it does **not** disable
/// preemption or IRQ reentrancy by itself — that remains the caller's job
/// through the existing `kspin` guards.
///
/// # Safety
/// `current_cpu` must report the logical CPU the current execution context is
/// actually pinned to, and that must not change while the guard is alive.
/// A `base` override must name an initialized slot area for that same CPU and
/// remain valid as long as the guard is alive. The default reads the
/// architecture per-CPU base register at access time, so a guard that does not
/// override it never snapshots a base that could go stale.
pub unsafe trait PinCurrentCpu {
    /// The logical CPU this context is pinned to.
    fn current_cpu(&self) -> CpuId;

    /// The per-CPU area base for this pinned context.
    ///
    /// The default reads the architecture base register. Guards that captured
    /// the base when they were created may override this to avoid a redundant
    /// read on every access; that value must refer to the same CPU reported by
    /// [`Self::current_cpu`].
    fn base(&self) -> usize {
        crate::arch::current_base()
    }
}

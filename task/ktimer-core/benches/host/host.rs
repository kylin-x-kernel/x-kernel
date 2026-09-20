// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Host-only CPU ID and BaseGuard contracts; no topology or IRQ emulation.

/// Identifies a logical timer owner in the host model, not a host processor.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct LogicalCpuId(usize);

impl LogicalCpuId {
    /// Wraps an index without validating CPU topology or timer-set bounds.
    pub const fn new(id: usize) -> Self {
        Self(id)
    }

    /// Returns the logical owner index.
    pub const fn as_usize(self) -> usize {
        self.0
    }
}

impl From<usize> for LogicalCpuId {
    fn from(id: usize) -> Self {
        Self(id)
    }
}

/// Supplies the guard contract needed by the imported `kspin` lock source.
///
/// Host builds use no-op context guards: they neither mask interrupts nor
/// disable preemption. The imported spinlock still performs real atomic locking.
pub trait BaseGuard {
    /// Saved execution-context state passed back to `release`.
    type State: Clone + Copy;
    /// Enters the guarded context and returns its previous state.
    fn acquire() -> Self::State;
    /// Leaves the guarded context using the state returned by `acquire`.
    fn release(state: Self::State);
}

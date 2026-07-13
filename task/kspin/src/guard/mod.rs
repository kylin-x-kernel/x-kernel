// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! RAII guards for critical sections with IRQ/preemption control.
//!
//! This module provides a composable guard system for implementing
//! kernel spinlocks with different synchronization guarantees.

/// Low-level kernel interfaces for guard operations.
#[kiface::interface]
pub trait KernelGuardIf {
    /// Enable kernel preemption.
    fn enable_preempt();

    /// Disable kernel preemption.
    fn disable_preempt();
}

/// Base trait for all guard types.
///
/// Guards implement RAII pattern to automatically manage critical sections.
pub trait BaseGuard {
    /// State saved when entering critical section.
    type State: Clone + Copy;

    /// Enter critical section, returning saved state.
    fn acquire() -> Self::State;

    /// Exit critical section, restoring state.
    fn release(state: Self::State);
}

mod arch;
mod types;

pub use types::{IrqSave, NoOp, NoPreempt, NoPreemptIrqSave};

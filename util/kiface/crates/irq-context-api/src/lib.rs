// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IRQ, NMI and bottom-half execution-context observations.

#![no_std]

/// IRQ context observations; callers retain exclusion across dependent actions.
/// No allocation, scheduling or cleanup checks are permitted by these queries.
#[kiface::interface]
pub trait InterruptContextIf {
    /// Reports effective NMI nesting without locks, guards or task access.
    fn is_in_nmi() -> bool;

    /// Reports NMI, hardirq, serving softirq, or task-context BH exclusion.
    ///
    /// Test NMI before any ordinary context lock. Outside NMI, protect the
    /// per-CPU snapshot without running a scheduler check on guard release.
    fn is_in_interrupt_context() -> bool;

    /// Reports an executing NMI, hardirq or softirq handler, excluding BH-only tasks.
    ///
    /// In NMI this must return without locks, guards or task access. Outside
    /// NMI, preserve the same snapshot protection as is_in_interrupt_context.
    fn is_in_interrupt_handler() -> bool;
}

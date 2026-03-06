// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Architecture-specific IRQ save/restore helpers.
//!
//! Delegates to [`karch`] for a unified implementation across all supported
//! architectures.
#![cfg_attr(not(target_os = "none"), allow(dead_code))]

/// Saves and disables local interrupts, returning the saved state.
#[inline]
pub fn save_disable() -> usize {
    karch::save_irq_and_disable()
}

/// Restores local interrupt state from the saved flags.
#[inline]
pub fn restore(flags: usize) {
    karch::restore_irq(flags)
}

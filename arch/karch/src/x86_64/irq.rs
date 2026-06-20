// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Interrupt control operations for x86_64.

#[cfg(target_os = "none")]
use core::arch::asm;

use x86_64::instructions::interrupts;

/// Allows the current CPU to respond to interrupts.
#[inline]
pub fn enable_local_irq() {
    #[cfg(target_os = "none")]
    interrupts::enable()
}

/// Makes the current CPU ignore interrupts.
#[inline]
pub fn disable_local_irq() {
    #[cfg(target_os = "none")]
    interrupts::disable()
}

/// Returns whether the current CPU is allowed to respond to interrupts.
#[inline]
pub fn local_irq_enabled() -> bool {
    interrupts::are_enabled()
}

/// Deprecated: use [`enable_local_irq`] instead.
#[deprecated(note = "Use `enable_local_irq` instead")]
#[inline]
pub fn enable_irq() {
    enable_local_irq()
}

/// Deprecated: use [`disable_local_irq`] instead.
#[deprecated(note = "Use `disable_local_irq` instead")]
#[inline]
pub fn disable_irq() {
    disable_local_irq()
}

/// Deprecated: use [`local_irq_enabled`] instead.
#[deprecated(note = "Use `local_irq_enabled` instead")]
#[inline]
pub fn irq_enabled() -> bool {
    local_irq_enabled()
}

/// Saves the current local interrupt state and disables interrupts atomically.
///
/// Returns the saved EFLAGS value with the IF bit. Pass it to [`restore_irq`]
/// to restore the previous interrupt state.
#[inline]
pub fn save_irq_and_disable() -> usize {
    #[cfg(target_os = "none")]
    {
        /// Interrupt Enable Flag (IF).
        const IF_BIT: usize = 1 << 9;
        let flags: usize;
        // SAFETY: captures the current flags and disables maskable interrupts
        // on the local CPU in one serialized sequence.
        unsafe { asm!("pushf; pop {}; cli", out(reg) flags) };
        flags & IF_BIT
    }
    #[cfg(not(target_os = "none"))]
    0
}

/// Restores local interrupt state from a value previously returned by
/// [`save_irq_and_disable`].
#[inline]
pub fn restore_irq(flags: usize) {
    #[cfg(target_os = "none")]
    {
        if flags != 0 {
            // SAFETY: restores the saved local interrupt-enabled state.
            unsafe { asm!("sti") };
        } else {
            // SAFETY: restores the saved local interrupt-disabled state.
            unsafe { asm!("cli") };
        }
    }
    #[cfg(not(target_os = "none"))]
    let _ = flags;
}

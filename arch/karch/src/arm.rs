// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ARM low-level architecture operations.

use core::arch::asm;

/// Interrupt Disable bit (bit 7) in CPSR.
const IRQ_DISABLE_BIT: usize = 1 << 7;

/// Saves the current local interrupt state and disables interrupts atomically.
///
/// Returns the saved CPSR value with the IRQ disable bit. Pass it to
/// [`restore_irq`] to restore the previous interrupt state.
#[inline]
pub fn save_irq_and_disable() -> usize {
    let flags: usize;
    unsafe {
        asm!(
            "mrs {0}, cpsr",
            "cpsid i",
            out(reg) flags,
            options(nomem, nostack, preserves_flags)
        );
    }
    flags & IRQ_DISABLE_BIT
}

/// Restores local interrupt state from a value previously returned by
/// [`save_irq_and_disable`].
///
/// In ARM CPSR, bit 7 = 0 means IRQs enabled, bit 7 = 1 means IRQs disabled.
/// If the saved flags had bit 7 clear (IRQs were enabled), re-enable them.
#[inline]
pub fn restore_irq(flags: usize) {
    if flags & IRQ_DISABLE_BIT == 0 {
        // IRQ_DISABLE_BIT was clear in saved flags → IRQs were enabled before; re-enable them.
        unsafe {
            asm!("cpsie i", options(nomem, nostack));
        }
    }
    // If IRQ_DISABLE_BIT was set → IRQs were already disabled; leave them disabled.
}

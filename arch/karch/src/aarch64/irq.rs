// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Interrupt control operations for AArch64.

use core::arch::asm;

use aarch64_cpu::registers::{DAIF, Readable, Writeable};

/// Allows the current CPU to respond to interrupts (clears DAIF.I).
#[inline]
pub fn enable_local_irq() {
    DAIF.write(DAIF::I::Unmasked);
}

/// Makes the current CPU ignore interrupts (sets DAIF.I).
#[inline]
pub fn disable_local_irq() {
    DAIF.write(DAIF::I::Masked);
}

/// Returns whether the current CPU is allowed to respond to interrupts.
#[inline]
pub fn local_irq_enabled() -> bool {
    !DAIF.is_set(DAIF::I)
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
/// Returns the saved DAIF register value. Pass it to [`restore_irq`] to
/// restore the previous interrupt state.
#[inline]
pub fn save_irq_and_disable() -> usize {
    let flags: usize;
    unsafe {
        asm!("mrs {}, daif", out(reg) flags, options(nomem, nostack, preserves_flags));
        asm!("msr daifset, #2", options(nomem, nostack));
    }
    flags
}

/// Restores local interrupt state from a value previously returned by
/// [`save_irq_and_disable`].
#[inline]
pub fn restore_irq(flags: usize) {
    unsafe {
        asm!("msr daif, {}", in(reg) flags, options(nomem, nostack));
    }
}

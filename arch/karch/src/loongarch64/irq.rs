// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Interrupt control operations for LoongArch64.

use core::arch::asm;

use loongArch64::register::crmd;

/// Allows the current CPU to respond to interrupts.
#[inline]
pub fn enable_local_irq() {
    crmd::set_ie(true)
}

/// Makes the current CPU ignore interrupts.
#[inline]
pub fn disable_local_irq() {
    crmd::set_ie(false)
}

/// Returns whether the current CPU is allowed to respond to interrupts.
#[inline]
pub fn local_irq_enabled() -> bool {
    crmd::read().ie()
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
/// Returns the saved CRMD value with the IE bit. Pass it to [`restore_irq`]
/// to restore the previous interrupt state.
#[inline]
pub fn save_irq_and_disable() -> usize {
    /// Interrupt Enable bit mask in CRMD.
    const IE_MASK: usize = 1 << 2;
    let mut flags: usize = 0;
    // csrxchg atomically reads CRMD and clears the IE bit
    unsafe { asm!("csrxchg {}, {}, 0x0", inout(reg) flags, in(reg) IE_MASK) };
    flags & IE_MASK
}

/// Restores local interrupt state from a value previously returned by
/// [`save_irq_and_disable`].
#[inline]
pub fn restore_irq(flags: usize) {
    /// Interrupt Enable bit mask in CRMD.
    const IE_MASK: usize = 1 << 2;
    // csrxchg atomically restores the IE bit
    unsafe { asm!("csrxchg {}, {}, 0x0", in(reg) flags, in(reg) IE_MASK) };
}

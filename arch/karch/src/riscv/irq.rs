// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Interrupt control operations for RISC-V.

use riscv::register::sstatus;

/// Allows the current CPU to respond to interrupts.
#[inline]
pub fn enable_local_irq() {
    // SAFETY: this only sets the current hart's supervisor interrupt-enable bit.
    unsafe { sstatus::set_sie() }
}

/// Makes the current CPU ignore interrupts.
#[inline]
pub fn disable_local_irq() {
    // SAFETY: this only clears the current hart's supervisor interrupt-enable bit.
    unsafe { sstatus::clear_sie() }
}

/// Returns whether the current CPU is allowed to respond to interrupts.
#[inline]
pub fn local_irq_enabled() -> bool {
    sstatus::read().sie()
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
/// Returns the saved sstatus value with the SIE bit. Pass it to
/// [`restore_irq`] to restore the previous interrupt state.
#[inline]
pub fn save_irq_and_disable() -> usize {
    /// Supervisor Interrupt Enable bit in sstatus.
    const SIE_BIT: usize = 1 << 1;
    let flags: usize;
    // csrrc: atomically clear the SIE bit and return the old sstatus value
    // SAFETY: this single CSR instruction only touches the current hart's
    // `sstatus` SIE bit and returns the previous value atomically.
    unsafe { core::arch::asm!("csrrc {}, sstatus, {}", out(reg) flags, const SIE_BIT) };
    flags & SIE_BIT
}

/// Restores local interrupt state from a value previously returned by
/// [`save_irq_and_disable`].
#[inline]
pub fn restore_irq(flags: usize) {
    // csrrs: set the bits from `flags` back into sstatus
    // SAFETY: `flags` comes from `save_irq_and_disable`, so only the saved
    // interrupt-enable bit is written back to the current hart's `sstatus`.
    unsafe { core::arch::asm!("csrrs x0, sstatus, {}", in(reg) flags) };
}

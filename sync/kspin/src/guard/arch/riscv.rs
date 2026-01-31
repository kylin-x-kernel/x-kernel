// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! RISC-V IRQ save/restore helpers.
use core::arch::asm;

/// Bit 1: Supervisor Interrupt Enable
const SIE_BIT: usize = 1 << 1;

/// Save SIE and disable interrupts.
#[inline]
pub fn save_disable() -> usize {
    let flags: usize;
    // clear the `SIE` bit, and return the old CSR
    unsafe { asm!("csrrc {}, sstatus, {}", out(reg) flags, const SIE_BIT) };
    flags & SIE_BIT
}

/// Restore SIE according to saved flags.
#[inline]
pub fn restore(flags: usize) {
    // restore the `SIE` bit
    unsafe { asm!("csrrs x0, sstatus, {}", in(reg) flags) };
}

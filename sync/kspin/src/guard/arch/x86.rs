<<<<<<< HEAD
//! x86/x86_64 IRQ save/restore helpers.
=======
// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

>>>>>>> 62a4f63a (./init, io, mm, net, platforms, process, sync over)
use core::arch::asm;

/// Interrupt Enable Flag (IF)
const IF_BIT: usize = 1 << 9;

/// Save IF and disable interrupts.
#[inline]
pub fn save_disable() -> usize {
    let flags: usize;
    unsafe { asm!("pushf; pop {}; cli", out(reg) flags) };
    flags & IF_BIT
}

/// Restore IF according to saved flags.
#[inline]
pub fn restore(flags: usize) {
    if flags != 0 {
        unsafe { asm!("sti") };
    } else {
        unsafe { asm!("cli") };
    }
}

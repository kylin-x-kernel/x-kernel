<<<<<<< HEAD
//! LoongArch64 IRQ save/restore helpers.
=======
// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

>>>>>>> 62a4f63a (./init, io, mm, net, platforms, process, sync over)
use core::arch::asm;

const IE_MASK: usize = 1 << 2;

/// Save IE and disable interrupts.
#[inline]
pub fn save_disable() -> usize {
    let mut flags: usize = 0;
    // clear the `IE` bit, and return the old CSR
    unsafe { asm!("csrxchg {}, {}, 0x0", inout(reg) flags, in(reg) IE_MASK) };
    flags & IE_MASK
}

/// Restore IE according to saved flags.
#[inline]
pub fn restore(flags: usize) {
    // restore the `IE` bit
    unsafe { asm!("csrxchg {}, {}, 0x0", in(reg) flags, in(reg) IE_MASK) };
}

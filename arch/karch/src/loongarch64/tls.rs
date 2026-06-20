// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Thread-local storage operations for LoongArch64.

use core::arch::asm;

/// Reads the thread pointer of the current CPU (`$tp`).
///
/// It is used to implement TLS (Thread Local Storage).
#[inline]
pub fn read_thread_pointer() -> usize {
    let tp;
    // SAFETY: this reads the current CPU's `$tp` register into a general-purpose
    // output without touching memory.
    unsafe { asm!("move {}, $tp", out(reg) tp) };
    tp
}

/// Writes the thread pointer of the current CPU (`$tp`).
///
/// It is used to implement TLS (Thread Local Storage).
///
/// # Safety
///
/// This function is unsafe as it changes the CPU states.
#[inline]
pub unsafe fn write_thread_pointer(val: usize) {
    // SAFETY: the caller guarantees `val` is a valid TLS/thread-pointer value
    // to publish in the current CPU's `$tp` register.
    unsafe { asm!("move $tp, {}", in(reg) val) }
}

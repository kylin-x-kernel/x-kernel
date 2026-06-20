// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! CPU control operations for LoongArch64.

use super::irq::disable_local_irq;

/// Halt the current CPU.
#[inline]
pub fn stop_cpu() {
    disable_local_irq();
    // SAFETY: with local IRQs disabled, putting the current CPU into the
    // LoongArch idle instruction is the intended terminal halt path.
    unsafe { loongArch64::asm::idle() }
}

/// Relaxes the current CPU and waits for interrupts.
///
/// It must be called with interrupts enabled, otherwise it will never return.
#[inline]
pub fn await_interrupts() {
    // SAFETY: this executes the architected idle instruction on the current CPU
    // and relies on the caller keeping interrupts enabled so wakeup is possible.
    unsafe { loongArch64::asm::idle() }
}

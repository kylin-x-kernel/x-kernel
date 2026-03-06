// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! CPU control operations for RISC-V.

use super::irq::disable_local_irq;

/// Halt the current CPU.
#[inline]
pub fn stop_cpu() {
    disable_local_irq();
    riscv::asm::wfi(); // should never return
}

/// Relaxes the current CPU and waits for interrupts.
///
/// It must be called with interrupts enabled, otherwise it will never return.
#[inline]
pub fn await_interrupts() {
    riscv::asm::wfi()
}

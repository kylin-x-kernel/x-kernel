// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! CPU control operations for RISC-V.

use super::irq::disable_local_irq;

/// Halt the current CPU terminally.
///
/// Disables local interrupts and remains in `wfi` until the hart is
/// reset. `wfi` may resume on events that are disabled, so the
/// instruction is re-entered rather than executed once. This is a
/// terminal operation and never returns.
#[inline]
pub fn stop_cpu() -> ! {
    disable_local_irq();
    loop {
        riscv::asm::wfi();
    }
}

/// Relaxes the current CPU and waits for interrupts.
///
/// It must be called with interrupts enabled, otherwise it will never return.
#[inline]
pub fn await_interrupts() {
    riscv::asm::wfi()
}

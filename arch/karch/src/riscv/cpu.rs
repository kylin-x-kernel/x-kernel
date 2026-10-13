// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! CPU control operations for RISC-V.

use super::irq::disable_local_irq;

/// Wait for an idle wakeup, returning with normal IRQs disabled.
///
/// # Safety
/// The caller must disable supervisor IRQ delivery and preemption, keep the
/// individual wake sources enabled, and hold no scheduler locks.
#[inline]
pub unsafe fn idle_wait() {
    debug_assert!(!super::irq::local_irq_enabled());
    // SAFETY: SIE remains clear under the caller's exclusion. WFI resumes on
    // individually enabled pending interrupts irrespective of global SIE;
    // the fence orders preceding work before waiting without consuming it.
    unsafe {
        core::arch::asm!("fence iorw, iorw", "wfi", options(nostack));
    }
}

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

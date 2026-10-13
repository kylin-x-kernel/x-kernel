// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! CPU control operations for x86_64.

use core::arch::asm;

use super::irq::disable_local_irq;

/// Wait for an idle wakeup, returning with normal IRQs disabled.
///
/// Non-bare-metal builds only spin once and do not model IRQ state.
///
/// # Safety
/// On bare metal, the caller must disable IRQs and preemption, hold no locks needed
/// by interrupt handlers, and keep preemption disabled until after return.
#[inline]
pub unsafe fn idle_wait() {
    #[cfg(target_os = "none")]
    debug_assert!(!super::irq::local_irq_enabled());
    #[cfg(target_os = "none")]
    // SAFETY: The caller excludes scheduling and starts with IF clear. STI's
    // interrupt shadow covers HLT, so a pending IRQ cannot be consumed before
    // the halt. CLI restores the entry mask before returning to the caller.
    unsafe {
        asm!("sti", "hlt", "cli", options(nostack));
    }
    #[cfg(not(target_os = "none"))]
    core::hint::spin_loop();
}

/// Halt the current CPU terminally.
///
/// Disables maskable interrupts and remains in `hlt` (the host-test build
/// spins) until the CPU is reset. `hlt` can still be exited by an NMI or
/// SMI, so the instruction is re-entered rather than executed once. This
/// is a terminal operation and never returns.
#[inline]
pub fn stop_cpu() -> ! {
    disable_local_irq();
    loop {
        if cfg!(target_os = "none") {
            // SAFETY: This terminal bare-metal loop deliberately keeps IF
            // clear and reenters HLT if an NMI or SMI resumes execution.
            unsafe { asm!("hlt") }
        } else {
            core::hint::spin_loop()
        }
    }
}

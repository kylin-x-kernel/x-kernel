// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! CPU control operations for x86_64.

use core::arch::asm;

use super::irq::disable_local_irq;

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
        await_interrupts();
    }
}

/// Relaxes the current CPU and waits for interrupts.
///
/// It must be called with interrupts enabled, otherwise it will never return.
#[inline]
pub fn await_interrupts() {
    if cfg!(target_os = "none") {
        // SAFETY: `hlt` is executed only in the bare-metal target path and is
        // intended to block until the next external interrupt arrives.
        unsafe { asm!("hlt") }
    } else {
        core::hint::spin_loop()
    }
}

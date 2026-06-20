// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! CPU control operations for x86_64.

use core::arch::asm;

use super::irq::disable_local_irq;

/// Halt the current CPU.
#[inline]
pub fn stop_cpu() {
    disable_local_irq();
    await_interrupts(); // should never return
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

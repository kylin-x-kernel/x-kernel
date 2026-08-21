// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! CPU control operations for AArch64.

/// Halt the current CPU terminally.
///
/// Masks every DAIF exception class — debug exceptions, SError, IRQs
/// (including PMR-mode pseudo-NMIs), and FIQs — then remains in the
/// architected wait state until the CPU is reset. This is a terminal
/// operation and never returns.
#[inline]
pub fn stop_cpu() -> ! {
    // Masking all four DAIF classes deliberately breaks the PMR-mode rule
    // that keeps `DAIF.I` clear (see `irq.rs`), because this one-way park
    // path must also block the pseudo-NMIs, SError, and debug exceptions
    // that could otherwise resume the CPU and panic it into a power-off.
    // Nothing runs on this CPU afterwards, so no restore path is needed.
    super::irq::disable_local_exceptions();
    loop {
        aarch64_cpu::asm::wfi();
    }
}

/// Relaxes the current CPU and waits for interrupts.
///
/// It must be called with interrupts enabled, otherwise it will never return.
#[inline]
pub fn await_interrupts() {
    aarch64_cpu::asm::wfi();
}

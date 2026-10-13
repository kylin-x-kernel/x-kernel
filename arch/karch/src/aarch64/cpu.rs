// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! CPU control operations for AArch64.

/// Wait for an idle wakeup, returning with normal IRQs disabled.
///
/// # Safety
/// The caller must disable normal IRQs and preemption, hold no scheduler
/// locks, and enter with the task-context NMI mask state. Wake sources must
/// remain enabled at the controller.
#[inline]
pub unsafe fn idle_wait() {
    debug_assert!(!super::irq::local_irq_enabled());
    #[cfg(feature = "nmi-pseudo")]
    if super::irq::pmr::is_ready() {
        // SAFETY: PMR readiness proves the GIC CPU interface is accessible.
        // Mask CPU delivery before opening PMR for wakeup; restore PMR before
        // DAIF. No ordinary IRQ helper may run inside this temporary state.
        unsafe {
            core::arch::asm!(
                "mrs {daif}, daif",
                "msr daifset, #3",
                "mrs {pmr}, ICC_PMR_EL1",
                "msr ICC_PMR_EL1, {open}",
                "isb",
                "dsb sy",
                "wfi",
                "msr ICC_PMR_EL1, {pmr}",
                "isb",
                "msr daif, {daif}",
                daif = out(reg) _,
                pmr = out(reg) _,
                open = in(reg) u64::from(super::irq::pmr::ALL),
                options(nostack, preserves_flags),
            );
        }
        return;
    }
    // SAFETY: Normal IRQ delivery is masked but WFI observes pending wake
    // signals. DAIF and, when supported, ALLINT are left unchanged, so a
    // hardware NMI retains its task-context delivery and return semantics.
    unsafe {
        core::arch::asm!("dsb sy", "wfi", options(nostack, preserves_flags));
    }
}

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

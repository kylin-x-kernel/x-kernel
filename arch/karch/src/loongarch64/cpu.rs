// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! CPU control operations for LoongArch64.

use super::irq::disable_local_irq;

core::arch::global_asm!(
    ".section .text.karch_idle_wait, \"ax\"",
    ".balign 4",
    ".global __karch_idle_wait",
    ".type __karch_idle_wait, @function",
    "__karch_idle_wait:",
    "dbar 0",
    "ori $t0, $zero, {crmd_ie}",
    "csrxchg $t0, $t0, 0",
    ".global __karch_idle_instruction",
    "__karch_idle_instruction:",
    // Match Linux arch/loongarch/kernel/genex.S::__arch_cpu_idle's hint.
    // The existing terminal stop path retains its separate halt behavior.
    "idle 0",
    ".global __karch_idle_return",
    "__karch_idle_return:",
    "ori $t0, $zero, {crmd_ie}",
    "csrxchg $zero, $t0, 0",
    "jr $ra",
    ".size __karch_idle_wait, . - __karch_idle_wait",
    crmd_ie = const 1 << 2,
);

unsafe extern "C" {
    fn __karch_idle_wait();
    static __karch_idle_instruction: u8;
    static __karch_idle_return: u8;
}

/// Skip IDLE when a kernel IRQ has already consumed its wakeup.
///
/// The trap dispatcher applies this only to kernel IRQ frames. Matching the
/// exact instruction avoids rewriting unrelated exception return addresses.
#[inline]
pub fn fixup_idle_irq_return(pc: &mut usize) {
    if *pc == core::ptr::addr_of!(__karch_idle_instruction) as usize {
        *pc = core::ptr::addr_of!(__karch_idle_return) as usize;
    }
}

/// Wait for an idle wakeup, returning with normal IRQs disabled.
///
/// # Safety
/// The caller must disable normal IRQs and preemption, hold no locks needed
/// by interrupt handlers, and keep preemption disabled until after return.
/// Kernel IRQ entry must apply [`fixup_idle_irq_return`] before returning.
#[inline]
pub unsafe fn idle_wait() {
    debug_assert!(!super::irq::local_irq_enabled());
    // SAFETY: The caller excludes scheduling. The leaf preserves callee-saved
    // registers; IRQ return skips IDLE if interrupted after enabling IE.
    unsafe { __karch_idle_wait() };
}

/// Halt the current CPU terminally.
///
/// Disables local interrupts and remains in the LoongArch idle
/// instruction until the CPU is reset. The instruction may resume on
/// disabled events, so it is re-entered rather than executed once. This
/// is a terminal operation and never returns.
#[inline]
pub fn stop_cpu() -> ! {
    disable_local_irq();
    loop {
        // SAFETY: with local IRQs disabled, putting the current CPU into the
        // LoongArch idle instruction is the intended terminal halt path.
        unsafe { loongArch64::asm::idle() }
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! RISC-V per-CPU base: the `gp` register.
//!
//! `gp` matches the previous `percpu 0.4` integration in this tree (see
//! `percpu-0.4.0/src/imp.rs`). `tp` is not available here: the kernel's TLS
//! implementation already uses it (`arch/khal/src/tls.rs`). On bare-metal
//! kernels there is no active ABI global pointer discipline, so `gp` is free
//! for this purpose; if that ever changes, this module and the TLS mapping
//! must be revisited together.

/// Reads the per-CPU area base from `gp`.
#[inline]
pub(crate) fn current_base() -> usize {
    let value: usize;
    // SAFETY: Reads the architecture's designated per-CPU base register.
    unsafe {
        core::arch::asm!("mv {}, gp", out(reg) value, options(nostack, preserves_flags));
    }
    value
}

/// Installs `value` as this CPU's per-CPU area base.
///
/// # Safety
///
/// Call during setup of the current CPU with interrupts and migration
/// prevented and exclusive ownership of its per-CPU area. `value` must
/// identify that CPU's initialized per-CPU allocation, which must remain
/// mapped and live for all subsequent slot accesses.
#[cfg(not(test))]
#[inline]
pub(crate) unsafe fn set_current_base(value: usize) {
    // SAFETY: The caller exclusively owns the current CPU's area during early
    // CPU setup; this writes only the architecture base register. Trap frames
    // must never save or restore `gp` (see `arch/kcpu/src/riscv/macros.rs`).
    unsafe {
        core::arch::asm!("mv gp, {}", in(reg) value, options(nostack, preserves_flags));
    }
}

// Scalar single-instruction operations. These architectures have no
// per-CPU-base offset addressing instruction, so the operations mask local
// IRQs through `karch`'s `CpuLocalIrqIf` provider around a plain access.
crate::single_instr::impl_single_instr_fallback_arith!(
    u8, u16, u32, u64, usize, i8, i16, i32, i64, isize
);
crate::single_instr::impl_single_instr_fallback_load_store!(
    u8, u16, u32, u64, usize, i8, i16, i32, i64, isize, bool
);

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! AArch64 per-CPU base: the `TPIDR_EL1` register.
//!
//! This matches the previous `percpu 0.4` selection; the workspace built with
//! the `arm-el2` feature off, so EL1 was the per-CPU base's home.

/// Reads the per-CPU area base from `TPIDR_EL1`.
#[inline]
pub(crate) fn current_base() -> usize {
    let value: usize;
    // SAFETY: Reads the architecture's designated per-CPU base register.
    unsafe {
        core::arch::asm!("mrs {}, tpidr_el1", out(reg) value, options(nostack, preserves_flags));
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
    // CPU setup; this writes only the architecture base register.
    unsafe {
        core::arch::asm!("msr tpidr_el1, {}", in(reg) value, options(nostack, preserves_flags));
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

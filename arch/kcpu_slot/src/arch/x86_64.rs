// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! x86-64 per-CPU base: the `GS` base register (MSR `IA32_GS_BASE`).
//!
//! [`current_base`] reads a self-pointer slot at a fixed offset inside this
//! CPU's area through GS-relative addressing instead of `rdmsr`: a
//! GS-relative load is cheap enough for `CpuPin::base()` on every
//! slot access, while MSR reads are not. [`set_current_base`] installs the
//! area through the `x86_64` crate's typed MSR wrapper and does not assume
//! `CR4.FSGSBASE` is enabled.

// Only `set_current_base` touches the crate, and it is absent from test builds.
#[cfg(not(test))]
use x86_64::VirtAddr;
#[cfg(not(test))]
use x86_64::registers::model_specific::GsBase;

/// Reads the per-CPU area base from the GS-relative self-pointer slot.
#[inline]
pub(crate) fn current_base() -> usize {
    let value: usize;
    // SAFETY: This only loads the self-pointer slot inside this CPU's area;
    // it does not create a reference to the VMA-zero template symbol.
    unsafe {
        core::arch::asm!(
            "mov {0}, qword ptr gs:[offset {SELF}]",
            out(reg) value,
            SELF = sym crate::layout::CPU_SLOT_SELF_PTR,
            options(nostack, preserves_flags),
        );
    }
    value
}

/// Installs `value` as this CPU's per-CPU area base.
#[cfg(not(test))]
#[inline]
pub(crate) unsafe fn set_current_base(value: usize) {
    // The offset macro carries its own `unsafe` asm block, so it stays
    // outside this function's `unsafe` scope.
    let offset = crate::__cpu_slot_symbol_offset!(crate::layout::CPU_SLOT_SELF_PTR);
    // The self-pointer is stored through its plain address first, so the
    // write does not depend on the GS base being installed yet.
    // SAFETY: The caller supplied an exclusive, initialized CPU area, and
    // `CPU_SLOT_SELF_PTR` names a `usize` slot inside it.
    unsafe {
        ((value + offset) as *mut usize).write_unaligned(value);
    }
    // SAFETY: The typed MSR wrapper writes only `IA32_GS_BASE`.
    GsBase::write(VirtAddr::new_truncate(value as u64));
}

use crate::single_instr::{
    SingleInstructionAddAssign, SingleInstructionBitAndAssign, SingleInstructionBitOrAssign,
    SingleInstructionBitXorAssign, SingleInstructionLoad, SingleInstructionStore,
    SingleInstructionSubAssign,
};

/// Generates the true single-instruction `gs:[offset]` implementations for the
/// listed scalar types.
macro_rules! impl_single_instr_gs {
    ($([$ty:ty, $reg:ident, $fmt:expr])*) => {$(
        impl SingleInstructionAddAssign<$ty> for $ty {
            unsafe fn add_assign(offset: usize, rhs: Self) {
                // SAFETY: the caller guarantees `offset` names a live CPU-local
                // scalar; `gs` is this CPU's area (global invariant).
                unsafe {
                    core::arch::asm!(
                        concat!("add gs:[{0}], {1", $fmt, "}"),
                        in(reg) offset,
                        in($reg) rhs,
                        options(nostack),
                    );
                }
            }
        }

        impl SingleInstructionSubAssign<$ty> for $ty {
            unsafe fn sub_assign(offset: usize, rhs: Self) {
                // SAFETY: as `add_assign`.
                unsafe {
                    core::arch::asm!(
                        concat!("sub gs:[{0}], {1", $fmt, "}"),
                        in(reg) offset,
                        in($reg) rhs,
                        options(nostack),
                    );
                }
            }
        }

        impl SingleInstructionBitAndAssign<$ty> for $ty {
            unsafe fn bitand_assign(offset: usize, rhs: Self) {
                // SAFETY: as `add_assign`.
                unsafe {
                    core::arch::asm!(
                        concat!("and gs:[{0}], {1", $fmt, "}"),
                        in(reg) offset,
                        in($reg) rhs,
                        options(nostack),
                    );
                }
            }
        }

        impl SingleInstructionBitOrAssign<$ty> for $ty {
            unsafe fn bitor_assign(offset: usize, rhs: Self) {
                // SAFETY: as `add_assign`.
                unsafe {
                    core::arch::asm!(
                        concat!("or gs:[{0}], {1", $fmt, "}"),
                        in(reg) offset,
                        in($reg) rhs,
                        options(nostack),
                    );
                }
            }
        }

        impl SingleInstructionBitXorAssign<$ty> for $ty {
            unsafe fn bitxor_assign(offset: usize, rhs: Self) {
                // SAFETY: as `add_assign`.
                unsafe {
                    core::arch::asm!(
                        concat!("xor gs:[{0}], {1", $fmt, "}"),
                        in(reg) offset,
                        in($reg) rhs,
                        options(nostack),
                    );
                }
            }
        }

        impl SingleInstructionLoad for $ty {
            unsafe fn load(offset: usize) -> Self {
                let val: Self;
                // SAFETY: as `add_assign`.
                unsafe {
                    core::arch::asm!(
                        concat!("mov {0", $fmt, "}, gs:[{1}]"),
                        out($reg) val,
                        in(reg) offset,
                        options(nostack, readonly),
                    );
                }
                val
            }
        }

        impl SingleInstructionStore for $ty {
            unsafe fn store(offset: usize, val: Self) {
                // SAFETY: as `add_assign`.
                unsafe {
                    core::arch::asm!(
                        concat!("mov gs:[{0}], {1", $fmt, "}"),
                        in(reg) offset,
                        in($reg) val,
                        options(nostack),
                    );
                }
            }
        }
    )*};
}

impl_single_instr_gs!(
    [u64, reg, ":r"]
    [usize, reg, ":r"]
    [u32, reg, ":e"]
    [u16, reg, ":x"]
    [u8, reg_byte, ""]
    [i64, reg, ":r"]
    [isize, reg, ":r"]
    [i32, reg, ":e"]
    [i16, reg, ":x"]
    [i8, reg_byte, ""]
);

// Booleans occupy a byte whose other bits must stay zero.
impl SingleInstructionLoad for bool {
    unsafe fn load(offset: usize) -> Self {
        let val: u8;
        // SAFETY: as `add_assign` for the integer types.
        unsafe {
            core::arch::asm!(
                "mov {0}, gs:[{1}]",
                out(reg_byte) val,
                in(reg) offset,
                options(nostack, readonly),
            );
        }
        debug_assert!(val == 0 || val == 1);
        val == 1
    }
}

impl SingleInstructionStore for bool {
    unsafe fn store(offset: usize, val: Self) {
        let val: u8 = u8::from(val);
        // SAFETY: as `add_assign` for the integer types.
        unsafe {
            core::arch::asm!(
                "mov gs:[{0}], {1}",
                in(reg) offset,
                in(reg_byte) val,
                options(nostack),
            );
        }
    }
}

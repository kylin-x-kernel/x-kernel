// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Architecture-backed single-instruction per-CPU value operations.
//!
//! Reading or updating one CPU's copy of a small scalar can be a single
//! instruction on architectures with a per-CPU base register that supports
//! offset addressing (x86-64 `gs:[offset]`). Doing so avoids taking a guard or
//! masking interrupts on that CPU's hot path.
//!
//! Architectures without such an instruction implement these traits by saving
//! and disabling local IRQs through [`kiface_cpu_api::CpuLocalIrqIf`], performing a
//! plain read/modify/write, and restoring the mask. Both forms are only sound
//! for a scalar that is never borrowed and is accessed exclusively from the CPU
//! whose area the base register selects.
//!
//! # Safety
//!
//! Every method is `unsafe`: `offset` must be the link-time offset of a live
//! per-CPU slot of exactly this type, and no other thread/IRQ may alias it.
//! The operations are not atomic across CPUs.

/// Single-instruction `+=` on a per-CPU scalar.
pub trait SingleInstructionAddAssign<Rhs = Self> {
    /// Adds `rhs` to the per-CPU object, wrapping on overflow.
    ///
    /// # Safety
    /// `offset` must name a live per-CPU object of this type on the current CPU.
    unsafe fn add_assign(offset: usize, rhs: Rhs);
}

/// Single-instruction `-=` on a per-CPU scalar.
pub trait SingleInstructionSubAssign<Rhs = Self> {
    /// Subtracts `rhs` from the per-CPU object, wrapping on underflow.
    ///
    /// # Safety
    /// As [`SingleInstructionAddAssign::add_assign`].
    unsafe fn sub_assign(offset: usize, rhs: Rhs);
}

/// Single-instruction `|=` on a per-CPU scalar.
pub trait SingleInstructionBitOrAssign<Rhs = Self> {
    /// Bitwise-ORs `rhs` into the per-CPU object.
    ///
    /// # Safety
    /// As [`SingleInstructionAddAssign::add_assign`].
    unsafe fn bitor_assign(offset: usize, rhs: Rhs);
}

/// Single-instruction `&=` on a per-CPU scalar.
pub trait SingleInstructionBitAndAssign<Rhs = Self> {
    /// Bitwise-ANDs `rhs` into the per-CPU object.
    ///
    /// # Safety
    /// As [`SingleInstructionAddAssign::add_assign`].
    unsafe fn bitand_assign(offset: usize, rhs: Rhs);
}

/// Single-instruction `^=` on a per-CPU scalar.
pub trait SingleInstructionBitXorAssign<Rhs = Self> {
    /// Bitwise-XORs `rhs` into the per-CPU object.
    ///
    /// # Safety
    /// As [`SingleInstructionAddAssign::add_assign`].
    unsafe fn bitxor_assign(offset: usize, rhs: Rhs);
}

/// Single-instruction load of a per-CPU scalar.
pub trait SingleInstructionLoad {
    /// Loads the per-CPU object.
    ///
    /// # Safety
    /// As [`SingleInstructionAddAssign::add_assign`].
    unsafe fn load(offset: usize) -> Self;
}

/// Single-instruction store of a per-CPU scalar.
pub trait SingleInstructionStore {
    /// Stores `val` into the per-CPU object.
    ///
    /// # Safety
    /// As [`SingleInstructionAddAssign::add_assign`].
    unsafe fn store(offset: usize, val: Self);
}

#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: IRQ-save fallback macros are used only on architectures \
              lacking the corresponding single instruction"
)]
#[allow(
    unused_macros,
    reason = "CG-024-lint-exceptions: unused_macros: IRQ-save fallback macros are used only on \
              architectures lacking the corresponding single instruction"
)]
/// Generates the arithmetic `SingleInstruction*` impls with the IRQ-save
/// fallback for the listed integer types.
macro_rules! impl_single_instr_fallback_arith {
    ($($ty:ty),* $(,)?) => {$(
        impl crate::single_instr::SingleInstructionAddAssign<$ty> for $ty {
            unsafe fn add_assign(offset: usize, rhs: Self) {
                let state = kiface_cpu_api::CpuLocalIrqIf::save_and_disable();
                let base = crate::arch::current_base();
                let ptr = (base + offset) as *mut $ty;
                // SAFETY: the caller guarantees `offset` names a live per-CPU
                // scalar; local IRQs are masked, so no same-CPU handler aliases it.
                unsafe { ptr.write(ptr.read().wrapping_add(rhs)) };
                kiface_cpu_api::CpuLocalIrqIf::restore(state);
            }
        }

        impl crate::single_instr::SingleInstructionSubAssign<$ty> for $ty {
            unsafe fn sub_assign(offset: usize, rhs: Self) {
                let state = kiface_cpu_api::CpuLocalIrqIf::save_and_disable();
                let base = crate::arch::current_base();
                let ptr = (base + offset) as *mut $ty;
                // SAFETY: as `add_assign`.
                unsafe { ptr.write(ptr.read().wrapping_sub(rhs)) };
                kiface_cpu_api::CpuLocalIrqIf::restore(state);
            }
        }

        impl crate::single_instr::SingleInstructionBitOrAssign<$ty> for $ty {
            unsafe fn bitor_assign(offset: usize, rhs: Self) {
                let state = kiface_cpu_api::CpuLocalIrqIf::save_and_disable();
                let base = crate::arch::current_base();
                let ptr = (base + offset) as *mut $ty;
                // SAFETY: as `add_assign`.
                unsafe { ptr.write(ptr.read() | rhs) };
                kiface_cpu_api::CpuLocalIrqIf::restore(state);
            }
        }

        impl crate::single_instr::SingleInstructionBitAndAssign<$ty> for $ty {
            unsafe fn bitand_assign(offset: usize, rhs: Self) {
                let state = kiface_cpu_api::CpuLocalIrqIf::save_and_disable();
                let base = crate::arch::current_base();
                let ptr = (base + offset) as *mut $ty;
                // SAFETY: as `add_assign`.
                unsafe { ptr.write(ptr.read() & rhs) };
                kiface_cpu_api::CpuLocalIrqIf::restore(state);
            }
        }

        impl crate::single_instr::SingleInstructionBitXorAssign<$ty> for $ty {
            unsafe fn bitxor_assign(offset: usize, rhs: Self) {
                let state = kiface_cpu_api::CpuLocalIrqIf::save_and_disable();
                let base = crate::arch::current_base();
                let ptr = (base + offset) as *mut $ty;
                // SAFETY: as `add_assign`.
                unsafe { ptr.write(ptr.read() ^ rhs) };
                kiface_cpu_api::CpuLocalIrqIf::restore(state);
            }
        }
    )*};
}

#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: IRQ-save fallback macros are used only on architectures \
              lacking the corresponding single instruction"
)]
#[allow(
    unused_macros,
    reason = "CG-024-lint-exceptions: unused_macros: IRQ-save fallback macros are used only on \
              architectures lacking the corresponding single instruction"
)]
/// Generates the `load`/`store` impls with the IRQ-save fallback.
macro_rules! impl_single_instr_fallback_load_store {
    ($($ty:ty),* $(,)?) => {$(
        impl crate::single_instr::SingleInstructionLoad for $ty {
            unsafe fn load(offset: usize) -> Self {
                let state = kiface_cpu_api::CpuLocalIrqIf::save_and_disable();
                let base = crate::arch::current_base();
                // SAFETY: the caller guarantees `offset` names a live per-CPU
                // scalar; local IRQs are masked for the read.
                let val = unsafe { ((base + offset) as *const $ty).read() };
                kiface_cpu_api::CpuLocalIrqIf::restore(state);
                val
            }
        }

        impl crate::single_instr::SingleInstructionStore for $ty {
            unsafe fn store(offset: usize, val: Self) {
                let state = kiface_cpu_api::CpuLocalIrqIf::save_and_disable();
                let base = crate::arch::current_base();
                // SAFETY: as `load`.
                unsafe { ((base + offset) as *mut $ty).write(val) };
                kiface_cpu_api::CpuLocalIrqIf::restore(state);
            }
        }
    )*};
}

#[cfg(not(target_arch = "x86_64"))]
pub(crate) use impl_single_instr_fallback_arith;
#[cfg(not(target_arch = "x86_64"))]
pub(crate) use impl_single_instr_fallback_load_store;

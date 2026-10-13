// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

//! Architecture-backed typed per-CPU storage.
//!
//! Static templates live in `.cpu_slot.template`; boot code initializes one
//! area per CPU, and hot paths access the selected area through the
//! architecture base register.

extern crate alloc;

mod dynamic;
mod guard;
mod layout;
mod macros;
mod single_instr;
mod static_slot;

mod arch;

pub use dynamic::{CpuSlotChunk, DynamicCpuSlot, SlotInitError};
pub use guard::{BootCpuToken, CpuId, CpuLocalGuard, CpuPin};
#[cfg(not(test))]
pub use layout::cpu_area_base;
pub use layout::{
    CpuArea, CpuInitError, area, area_size, begin_cpu_init, current_area, stride, template_size,
};
pub use single_instr::{
    SingleInstructionAddAssign, SingleInstructionBitAndAssign, SingleInstructionBitOrAssign,
    SingleInstructionBitXorAssign, SingleInstructionLoad, SingleInstructionStore,
    SingleInstructionSubAssign,
};
pub use static_slot::{CpuSlot, CpuSlotCell, StaticSlotValue};

/// Platform contract for saving and disabling local IRQs around the
/// single-instruction fallback.
///
/// Architectures without a true single-instruction per-CPU access implement
/// [`SingleInstructionLoad`] and friends by masking local IRQs, touching the
/// value, and restoring the mask. `kcpu_slot` cannot own that logic because the
/// correct mask primitive (including AArch64's PMR/NMI handling) lives above it
/// in `karch`; the platform thus provides exactly one implementation.
#[kiface::interface]
pub trait CpuLocalIrqIf {
    /// Saves the current local IRQ state and disables local IRQs, returning an
    /// opaque token for [`Self::restore`].
    fn save_and_disable() -> usize;
    /// Restores the local IRQ state previously returned by
    /// [`Self::save_and_disable`].
    fn restore(state: usize);
}

#[cfg(test)]
#[path = "../host-tests/kcpu_slot_host.rs"]
mod kcpu_slot_host_tests;

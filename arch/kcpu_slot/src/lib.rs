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

mod dynamic;
mod guard;
mod layout;
mod macros;
mod static_slot;

mod arch;

pub use dynamic::{CpuSlotChunk, DynamicCpuSlot, SlotInitError};
pub use guard::{CpuId, PinCurrentCpu};
pub use layout::{area_size, initialize_cpu, stride, template_size};
pub use static_slot::{CpuSlot, CpuSlotCell, StaticSlotValue};

#[cfg(test)]
mod tests;

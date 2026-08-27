// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Architecture-independent Virtual Machine Monitor (VMM).
//!
//! Provides the core VMM abstractions: [`Vm`], [`Vcpu`], and the
//! [`arch::VmmArch`] trait that each architecture implements to supply
//! guest context save/restore, guest entry, and exit handling.
//!
//! The VMM follows the avatar-next model: each vCPU is a regular
//! kernel task, mixed into the normal scheduler.

#![no_std]

extern crate alloc;

pub mod arch;
pub mod mm;
pub mod selftest;
pub mod vcpu;
pub mod vcpu_state;
pub mod vdev;
pub mod vm;
pub mod vm_info;

pub use selftest::{vmm_selftest, vmm_selftest_guest_mem, vmm_selftest_multi_vm, vmm_selftest_smp};
pub use vcpu::{ExitAction, Vcpu, spawn_vcpu_thread};
pub use vcpu_state::{VcpuRunState, VcpuStats};
pub use vm::{Vm, VmConfig, VmRef, VmShared};
pub use vm_info::{VmInfo, dump_vm_info};

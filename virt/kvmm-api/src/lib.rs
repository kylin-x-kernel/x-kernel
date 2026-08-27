// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! kvmm control-plane API crate.
//!
//! The [`kvmm`] crate provides only the VMM *mechanism* — [`kvmm::Vm`],
//! [`kvmm::Vcpu`], guest memory, and the virtual-device substrate. This crate
//! layers *policy* on top: character devices that decide how a VM is created,
//! driven, and destroyed.
//!
//! The first such device is [`KvmmVmDevice`], an fd-bound VM instance device
//! whose VM lifetime is tied to an open file description rather than to a
//! transient `echo` process. See its documentation for the lifecycle model.
//!
//! Future KVM-compatible ioctl devices should live here too, keeping all
//! lifecycle-bearing logic out of the mechanism crate.

#![no_std]

extern crate alloc;

pub mod device;
mod loader;

pub use device::KvmmVmDevice;

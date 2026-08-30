// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! AArch64 platform implementation and peripheral drivers.
//!
//! This crate provides:
//! - Generic AArch64 device-tree platform implementation of the `kplat` HAL
//! - ARM64 common peripheral drivers (PSCI, SMCCC, KVM hypercalls, PMU, NMI)

#![no_std]
#![cfg(target_arch = "aarch64")]
#![cfg(k_plat_name = "kplat-aarch64")]

extern crate alloc;

#[macro_use]
extern crate kplat;
#[macro_use]
extern crate log;
// Force-link kernel_boot so that _start and boot code are included in the final binary.
extern crate kernel_boot;

pub mod peripherals;

mod dma;
mod init;
mod mmio;
mod power;
#[cfg(feature = "pmu")]
pmu_if_impl!();
#[cfg(feature = "nmi")]
nmi_if_impl!();
#[cfg(feature = "nmi-pmu")]
nmi_pmu_if_impl!();

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform support for kplat-x86_64.

#![no_std]
#![cfg(target_arch = "x86_64")]
#![cfg(k_plat_name = "kplat-x86_64")]

#[macro_use]
extern crate log;
#[macro_use]
extern crate kplat;
extern crate irq_driver as _;
extern crate kernel_boot;
mod init;
#[cfg(feature = "smp")]
mod mp;
mod peripherals;
mod power;

kplat::default_dma_if_impl!();
kplat::default_mmio_if_impl!();

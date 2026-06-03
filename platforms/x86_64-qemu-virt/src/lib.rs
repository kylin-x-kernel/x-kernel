// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform support for x86_64-qemu-virt.

#![no_std]
#![cfg(target_arch = "x86_64")]
#![cfg(k_plat_name = "x86_64-qemu-virt")]

#[macro_use]
extern crate log;
#[macro_use]
extern crate kplat;
extern crate irq_driver as _;
extern crate kernel_boot;
mod init;
#[cfg(feature = "smp")]
mod mp;
mod power;

struct DmaPlatformImpl;
struct MmioPlatformImpl;

kplat::default_dma_if_impl!(DmaPlatformImpl);
kplat::default_mmio_if_impl!(MmioPlatformImpl);

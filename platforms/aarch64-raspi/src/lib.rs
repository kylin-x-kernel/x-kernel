// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Raspberry Pi (AArch64) platform support.

#![no_std]
#![cfg(target_arch = "aarch64")]

#[macro_use]
extern crate kplat;
mod boot;
mod init;
mod power;
#[cfg(feature = "smp")]
mod mp;

struct DmaPlatformImpl;
struct MmioPlatformImpl;

kplat::default_dma_if_impl!(DmaPlatformImpl);
kplat::default_mmio_if_impl!(MmioPlatformImpl);

#[cfg(feature = "irq")]
kplat_aarch64_peripherals::irq_if_impl!(IntrManagerImpl);

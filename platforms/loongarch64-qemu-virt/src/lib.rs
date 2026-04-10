// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![cfg(target_arch = "loongarch64")]
#![no_std]
#[macro_use]
extern crate log;
#[macro_use]
extern crate kplat;
mod boot;
mod console;
mod init;
mod irq;
#[cfg(feature = "smp")]
mod mp;
mod power;
mod time;

struct DmaPlatformImpl;
struct MmioPlatformImpl;

kplat::default_dma_if_impl!(DmaPlatformImpl);
kplat::default_mmio_if_impl!(MmioPlatformImpl);

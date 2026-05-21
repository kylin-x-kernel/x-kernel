// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_std]
#![cfg(target_arch = "loongarch64")]

#[macro_use]
extern crate log;
#[macro_use]
extern crate kplat;
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

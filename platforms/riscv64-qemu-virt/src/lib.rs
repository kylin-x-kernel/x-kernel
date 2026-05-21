// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_std]
#![cfg(target_arch = "riscv64")]

#[macro_use]
extern crate log;
#[macro_use]
extern crate kplat;
// Force-link kernel_boot so that _start and boot code are included in the final binary.
extern crate kernel_boot;
mod init;
mod power;

struct DmaPlatformImpl;
struct MmioPlatformImpl;

kplat::default_dma_if_impl!(DmaPlatformImpl);
kplat::default_mmio_if_impl!(MmioPlatformImpl);

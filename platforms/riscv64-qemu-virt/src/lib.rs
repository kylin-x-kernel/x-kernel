// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![cfg(target_arch = "riscv64")]
#![no_std]
#[macro_use]
extern crate log;
#[macro_use]
extern crate kplat;
// Force-link kernel_boot so that _start and boot code are included in the final binary.
extern crate kernel_boot;
mod console;
mod init;
mod irq;
mod mem;
mod power;
mod time;

struct DmaPlatformImpl;

kplat::default_dma_if_impl!(DmaPlatformImpl);

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Raspberry Pi (AArch64) platform support.
#![no_std]
#[macro_use]
extern crate kplat;
mod boot;
mod init;
mod mem;
mod power;
#[cfg(feature = "smp")]
mod mp;
kplat_aarch64_peripherals::console_if_impl!(ConsoleImpl);
kplat_aarch64_peripherals::time_if_impl!(GlobalTimerImpl);
#[cfg(feature = "irq")]
kplat_aarch64_peripherals::irq_if_impl!(IntrManagerImpl);

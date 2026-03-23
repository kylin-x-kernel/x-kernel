// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![cfg(target_arch = "x86_64")]
#![no_std]
#[macro_use]
extern crate log;
#[macro_use]
extern crate kplat;
extern crate kernel_boot;
mod init;
mod mem;
#[cfg(feature = "smp")]
mod mp;
mod power;
pub mod psci;

x86_peripherals::console_if_impl!(ConsoleImpl, irq = None);
x86_peripherals::time_if_impl!(GlobalTimerImpl);
x86_peripherals::irq_if_impl!(IntrManagerImpl);

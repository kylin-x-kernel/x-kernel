// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform support for x86_64-qemu-virt.

#![cfg(target_arch = "x86_64")]
#![no_std]
#[macro_use]
extern crate log;
#[macro_use]
extern crate kplat;
mod acpi;
extern crate kernel_boot;
mod init;
mod mem;
#[cfg(feature = "smp")]
mod mp;
mod power;

x86_peripherals::console_if_impl!(ConsoleImpl, irq = Some(4));
x86_peripherals::time_if_impl!(GlobalTimerImpl);
x86_peripherals::irq_if_impl!(IntrManagerImpl);

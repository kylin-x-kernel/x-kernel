// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform support for the aarch64 crosvm-virt target.
#![no_std]
#[macro_use]
extern crate kplat;
extern crate kernel_boot;
mod gicv3;
mod init;
mod mem;
mod power;
pub mod psci;
aarch64_peripherals::ns16550_console_if_impl!(ConsoleImpl);
aarch64_peripherals::time_if_impl!(GlobalTimerImpl);
irq_if_impl!(IntrManagerImpl);

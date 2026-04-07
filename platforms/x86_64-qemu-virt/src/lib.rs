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

struct DmaPlatformImpl;
struct MmioPlatformImpl;

kplat::default_dma_if_impl!(DmaPlatformImpl);
kplat::default_mmio_if_impl!(MmioPlatformImpl);

x86_peripherals::console_if_impl!(ConsoleImpl, irq = Some(4));
timer_driver::x86_lapic_tsc_timer_if_impl!(GlobalTimerImpl);
x86_apic::irq_if_impl!(IntrManagerImpl);

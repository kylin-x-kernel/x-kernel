// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform support for aarch64-qemu-virt.

#![no_std]
#[macro_use]
extern crate kplat;
// Force-link kernel_boot so that _start and boot code are included in the final binary.
extern crate kernel_boot;

mod init;
mod mem;
mod power;

struct DmaPlatformImpl;
struct MmioPlatformImpl;

kplat::default_dma_if_impl!(DmaPlatformImpl);
kplat::default_mmio_if_impl!(MmioPlatformImpl);

aarch64_peripherals::console_if_impl!(ConsoleImpl);
aarch64_peripherals::time_if_impl!(GlobalTimerImpl);
aarch64_peripherals::irq_if_impl!(IntrManagerImpl);
#[cfg(feature = "pmu")]
aarch64_peripherals::pmu_if_impl!(PerfMgrImpl);
#[cfg(feature = "nmi")]
aarch64_peripherals::nmi_if_impl!(NmiIfImpl);

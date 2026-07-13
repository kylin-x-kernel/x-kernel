// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Raspberry Pi boot initialization hooks.
use crate::config::devices::TIMER_IRQ;
use khal::mem::{p2v, pa};
use kplat::boot::{BootHandler, BootInfo};

const GICD_PADDR: usize = 0xFF84_1000;
const GICC_PADDR: usize = 0xFF84_2000;
const UART_PADDR: usize = 0xFE20_1000;

#[impl_dev_interface]
impl BootHandler {
    fn early_init(_boot_info: &BootInfo) {
        kcpu::boot::init_trap();
        kplat_aarch64_peripherals::pl011::early_init(p2v(pa!(UART_PADDR)));
        kplat_aarch64_peripherals::generic_timer::early_init();
    }
    #[cfg(feature = "smp")]
    fn early_init_secondary(_cpu_id: usize) {
        kcpu::boot::init_trap();
    }
    fn final_init(_boot_info: &BootInfo) {
        #[cfg(feature = "irq")]
        {
            kplat_aarch64_peripherals::gic::init_gic(p2v(pa!(GICD_PADDR)), p2v(pa!(GICC_PADDR)));
            kplat_aarch64_peripherals::gic::init_gicc();
            kplat_aarch64_peripherals::generic_timer::enable_local(TIMER_IRQ);
        }
    }
    #[cfg(feature = "smp")]
    fn final_init_secondary(_cpu_id: usize) {
        #[cfg(feature = "irq")]
        {
            kplat_aarch64_peripherals::gic::init_gicc();
            kplat_aarch64_peripherals::generic_timer::enable_local(TIMER_IRQ);
        }
    }
}

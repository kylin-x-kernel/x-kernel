// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform initialization hooks for aarch64-qemu-virt.

#[allow(unused_imports)]
use kbuild_config::{PSCI_METHOD, RTC_PADDR, TIMER_IRQ, UART_PADDR};
use kplat::{
    boot::{BootHandler, BootInfo},
    memory::{p2v, pa},
};
struct BootHandlerImpl;
#[impl_dev_interface]
impl BootHandler for BootHandlerImpl {
    fn early_init(boot_info: &BootInfo) {
        crate::mem::early_init(boot_info.dtb_addr, boot_info.kernel_load_paddr);
        aarch64_peripherals::pl011::early_init(p2v(pa!(UART_PADDR)));
        aarch64_peripherals::psci::init(PSCI_METHOD);
        aarch64_peripherals::generic_timer::early_init();
        #[cfg(feature = "rtc")]
        aarch64_peripherals::pl031::early_init(p2v(pa!(RTC_PADDR)));
    }

    #[cfg(feature = "smp")]
    fn early_init_ap(_cpu_id: usize) {}

    fn final_init(_boot_info: &BootInfo) {
        aarch64_peripherals::gic::init_from_device_tree();
        aarch64_peripherals::generic_timer::enable_local(TIMER_IRQ);
    }

    #[cfg(feature = "smp")]
    fn final_init_ap(_cpu_id: usize) {
        aarch64_peripherals::gic::init_current_cpu();
        aarch64_peripherals::generic_timer::enable_local(TIMER_IRQ);
    }
}

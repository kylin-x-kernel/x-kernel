// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform initialization hooks for aarch64-qemu-virt.

#[allow(unused_imports)]
use kbuild_config::{PSCI_METHOD, RTC_PADDR, UART_PADDR};
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
        let timer_config = timer_driver::arm_generic::config_from_device_tree()
            .expect("ARM generic timer DT node is required on aarch64-qemu-virt");
        timer_driver::arm_generic::init(timer_config);
        #[cfg(feature = "rtc")]
        rtc_driver::init(
            rtc_driver::RtcConfig::mmio_mapped(
                rtc_driver::RtcKind::Pl031,
                p2v(pa!(RTC_PADDR)),
                rtc_driver::RtcSource::PlatformStatic,
            ),
            timer_driver::arm_generic::t2ns(timer_driver::arm_generic::now_ticks()),
        );
    }

    #[cfg(feature = "smp")]
    fn early_init_ap(_cpu_id: usize) {}

    fn final_init(_boot_info: &BootInfo) {
        aarch64_peripherals::gic::init_from_device_tree();
        timer_driver::arm_generic::init_percpu();
    }

    #[cfg(feature = "smp")]
    fn final_init_ap(_cpu_id: usize) {
        aarch64_peripherals::gic::init_current_cpu();
        timer_driver::arm_generic::init_percpu();
    }
}

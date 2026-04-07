// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kaddr_layout::PAGE_OFFSET;
use kbuild_config::RTC_PADDR;
use kplat::{
    boot::{BootHandler, BootInfo},
    memory::VirtAddr,
};

struct BootHandlerImpl;
#[impl_dev_interface]
impl BootHandler for BootHandlerImpl {
    fn early_init(boot_info: &BootInfo) {
        crate::mem::early_init(boot_info.dtb_addr, boot_info.kernel_load_paddr);
        crate::console::early_init();
        timer_driver::riscv_sbi::init(timer_driver::riscv_sbi::TimerConfig::platform_static(
            crate::irq::S_TIMER,
            10_000_000,
        ));
        #[cfg(feature = "rtc")]
        if RTC_PADDR != 0 {
            rtc_driver::init(
                rtc_driver::RtcConfig::mmio_mapped(
                    rtc_driver::RtcKind::Goldfish,
                    VirtAddr::from_usize(PAGE_OFFSET + RTC_PADDR),
                    rtc_driver::RtcSource::PlatformStatic,
                ),
                timer_driver::riscv_sbi::rtc_now_nanos(),
            );
        }
    }

    #[cfg(feature = "smp")]
    fn early_init_ap(_cpu_id: usize) {}

    fn final_init(_boot_info: &BootInfo) {
        crate::irq::init_percpu();
        timer_driver::riscv_sbi::init_percpu();
    }

    #[cfg(feature = "smp")]
    fn final_init_ap(_cpu_id: usize) {
        crate::irq::init_percpu();
        timer_driver::riscv_sbi::init_percpu();
    }
}

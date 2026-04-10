// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kplat::boot::{BootHandler, BootInfo};

struct BootHandlerImpl;
#[impl_dev_interface]
impl BootHandler for BootHandlerImpl {
    fn prepare_boot_memory(boot_info: &BootInfo) {
        let _ = boot_info;
    }

    fn firmware_init(_boot_info: &BootInfo) {}

    fn early_driver_init() {
        timer_driver::riscv_sbi::init(timer_driver::riscv_sbi::TimerConfig::platform_static(
            irq_driver::riscv::S_TIMER,
            10_000_000,
        ));
        irq_driver::riscv::init_primary();
        irq_driver::riscv::init_current_cpu_context();
        console_driver::init_from_device_tree().expect("failed to parse console from device tree");
        console_driver::register_input_irq_handler();
        #[cfg(feature = "rtc")]
        rtc_driver::init_from_device_tree(timer_driver::riscv_sbi::rtc_now_nanos())
            .expect("failed to parse rtc from device tree");
    }

    fn final_init(_boot_info: &BootInfo) {
        irq_driver::riscv::init_percpu();
        timer_driver::riscv_sbi::init_percpu();
    }

    #[cfg(feature = "smp")]
    fn final_init_ap(_cpu_id: usize) {
        irq_driver::riscv::init_percpu();
        timer_driver::riscv_sbi::init_percpu();
    }
}

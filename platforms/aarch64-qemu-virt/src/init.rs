// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform initialization hooks for aarch64-qemu-virt.

use kbuild_config::PSCI_METHOD;
use kplat::boot::{BootHandler, BootInfo};

struct BootHandlerImpl;
#[impl_dev_interface]
impl BootHandler for BootHandlerImpl {
    fn prepare_boot_memory(_boot_info: &BootInfo) {
        crate::mmio::prepare_boot_memory();
    }

    fn firmware_init(_boot_info: &BootInfo) {
        aarch64_peripherals::psci::init(PSCI_METHOD);
    }

    fn early_driver_init() {
        let timer_config = timer_driver::arm_generic::config_from_device_tree()
            .expect("ARM generic timer DT node is required on aarch64-qemu-virt");
        timer_driver::arm_generic::init(timer_config);
        console_driver::init_from_device_tree().expect("failed to parse console from device tree");
        #[cfg(feature = "rtc")]
        rtc_driver::init_from_device_tree(timer_driver::arm_generic::t2ns(
            timer_driver::arm_generic::now_ticks(),
        ))
        .expect("failed to parse rtc from device tree");
        irq_driver::gic::init_from_device_tree();
        console_driver::register_input_irq_handler();
    }

    fn final_init(_boot_info: &BootInfo) {
        irq_driver::gic::init_current_cpu();
        timer_driver::arm_generic::init_percpu();
    }

    #[cfg(feature = "smp")]
    fn final_init_ap(_cpu_id: kcpu_id_map::LogicalCpuId) {
        irq_driver::gic::init_current_cpu();
        timer_driver::arm_generic::init_percpu();
    }
}

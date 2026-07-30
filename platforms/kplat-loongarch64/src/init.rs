// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kplat::boot::{BootHandler, BootInfo};

#[impl_dev_interface]
impl BootHandler {
    fn prepare_boot_memory(_boot_info: &BootInfo) {}

    fn firmware_init(_boot_info: &BootInfo) {}

    fn early_driver_init() {
        crate::time::early_init();
        crate::irq::init();
        console_driver::init_stdout_from_device_tree()
            .expect("failed to parse console from device tree");
        console_driver::register_input_irq_handler();
    }

    fn final_init(_boot_info: &BootInfo) {
        crate::time::init_percpu();
    }

    #[cfg(feature = "smp")]
    fn final_init_ap(_logical_cpu_id: kcpu_id_map::LogicalCpuId) {
        crate::time::init_percpu();
    }
}

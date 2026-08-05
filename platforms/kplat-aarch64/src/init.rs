// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform initialization hooks for aarch64.

use kbuild_config::PSCI_METHOD;
use kplat::boot::{BootHandler, BootInfo};

#[impl_dev_interface]
impl BootHandler {
    fn prepare_boot_memory(_boot_info: &BootInfo) {
        crate::mmio::prepare_boot_memory();
    }

    fn firmware_init(_boot_info: &BootInfo) {
        // PSCI conduit method ("smc"/"hvc") from the /psci FDT node, falling
        // back to the Kconfig default when the node is absent.
        let method = of::find_node("/psci")
            .and_then(|node| node.property_str("method"))
            .unwrap_or(PSCI_METHOD);
        crate::peripherals::psci::init(method);
    }

    fn early_driver_init() {
        let timer_config = timer_driver::arm_generic::config_from_device_tree()
            .expect("ARM generic timer DT node is required on aarch64");
        timer_driver::arm_generic::init(timer_config);
        console_driver::init_stdout_from_device_tree()
            .expect("failed to parse console from device tree");
        #[cfg(feature = "rtc")]
        {
            // RTC init is best-effort: RK3588's RTC is a PMIC (rk806/hym8563
            // over i2c), not the pl031 MMIO RTC this driver probes, so it
            // typically finds nothing. Don't panic the whole boot over a
            // missing RTC; just record it for bring-up diagnostics.
            if let Some(sample) = rtc_driver::read_from_device_tree() {
                ktime::initialize_realtime(sample);
            } else {
                kernel_boot::bootln!("rtc init skipped (non-fatal): no pl031 RTC in device tree");
            }
        }
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

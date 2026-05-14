// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform boot hooks for early and final initialization.
use kbuild_config::PSCI_METHOD;
use kcpu_id_map::LogicalCpuId;
use kplat::boot::{BootHandler, BootInfo};
use log::*;

fn map_kvm_guarded_mmio() {
    crate::psci::kvm_guard_granule_init();
}

struct BootHandlerImpl;
#[impl_dev_interface]
impl BootHandler for BootHandlerImpl {
    fn prepare_boot_memory(boot_info: &BootInfo) {
        map_kvm_guarded_mmio();
        let _ = boot_info;
    }

    fn firmware_init(_boot_info: &BootInfo) {
        aarch64_peripherals::psci::init(PSCI_METHOD);
    }

    fn early_driver_init() {
        let timer_config = timer_driver::arm_generic::config_from_device_tree()
            .expect("ARM generic timer DT node is required on aarch64-crosvm-virt");
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

    /// Finish platform init after core subsystems are online.
    fn final_init(boot_info: &BootInfo) {
        info!("cpu_id {}", boot_info.cpu_id.as_usize());
        irq_driver::gic::init_current_cpu();
        timer_driver::arm_generic::init_percpu();
    }

    #[cfg(feature = "smp")]
    /// Finalize per-CPU setup on secondary cores.
    fn final_init_ap(_logical_cpu_id: LogicalCpuId) {
        irq_driver::gic::init_current_cpu();
        timer_driver::arm_generic::init_percpu();
    }
}

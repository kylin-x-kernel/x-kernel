// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform boot hooks for early and final initialization.
#[allow(unused_imports)]
use kbuild_config::{PSCI_METHOD, RTC_PADDR, TIMER_IRQ, UART_IRQ, UART_PADDR};
use kplat::{
    boot::{BootHandler, BootInfo},
    memory::{PhysAddr, p2v, pa},
};
use log::*;

const GICD_PADDR: usize = 0x3fff_0000;
const GICR_PADDR: usize = 0x3ffb_0000;
const GICD_MMIO_SIZE: usize = 0x1_0000;
const GICR_MMIO_SIZE: usize = 0x20_0000;

fn map_kvm_guarded_mmio() {
    crate::psci::kvm_guard_granule_init();
    crate::psci::do_xmap_granules(0x7200_0000, 0x100_0000);
    crate::psci::do_xmap_granules(0x7000_0000, 0x200_0000);
    crate::psci::do_xmap_granules(0x3ffb_0000, 0x20_0000);
    crate::psci::do_xmap_granules(0x2000, 0x1000);
}

struct BootHandlerImpl;
#[impl_dev_interface]
impl BootHandler for BootHandlerImpl {
    /// Perform early, minimal init before the allocator is ready.
    fn early_init(boot_info: &BootInfo) {
        let dtb = boot_info.dtb_addr;
        map_kvm_guarded_mmio();
        crate::mem::early_init(dtb, boot_info.kernel_load_paddr);
        aarch64_peripherals::ns16550a::early_init(p2v(pa!(UART_PADDR)));
        aarch64_peripherals::psci::init(PSCI_METHOD);
        aarch64_peripherals::generic_timer::early_init();
        #[cfg(feature = "rtc")]
        aarch64_peripherals::pl031::early_init(p2v(pa!(RTC_PADDR)));
    }

    #[cfg(feature = "smp")]
    fn early_init_ap(_cpu_id: usize) {}

    /// Finish platform init after core subsystems are online.
    fn final_init(boot_info: &BootInfo) {
        info!("cpu_id {}", boot_info.cpu_id);
        // Crosvm's DT description is not reliable enough for GIC discovery yet,
        // so keep the controller resource description static for now.
        let gic = aarch64_peripherals::gic::GicConfig {
            version: aarch64_peripherals::gic::GicVersion::V3,
            gicd: aarch64_peripherals::gic::GicMmioRegion {
                paddr: PhysAddr::from_usize(GICD_PADDR),
                size: GICD_MMIO_SIZE,
            },
            gicc: None,
            gicr: Some(aarch64_peripherals::gic::GicMmioRegion {
                paddr: PhysAddr::from_usize(GICR_PADDR),
                size: GICR_MMIO_SIZE,
            }),
        };
        aarch64_peripherals::gic::init(gic);
        info!("set UART IRQ {} as edge trigger", UART_IRQ);
        aarch64_peripherals::gic::set_trigger(UART_IRQ, true);
        aarch64_peripherals::generic_timer::enable_local(TIMER_IRQ);
    }

    #[cfg(feature = "smp")]
    /// Finalize per-CPU setup on secondary cores.
    fn final_init_ap(_cpu_id: usize) {
        aarch64_peripherals::gic::init_current_cpu();
        aarch64_peripherals::generic_timer::enable_local(TIMER_IRQ);
    }
}

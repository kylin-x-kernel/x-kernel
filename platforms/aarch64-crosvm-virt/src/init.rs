// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform boot hooks for early and final initialization.
#[allow(unused_imports)]
use kbuild_config::{
    GICC_PADDR as GICR_PADDR, GICD_PADDR, PSCI_METHOD, RTC_PADDR, TIMER_IRQ, UART_IRQ, UART_PADDR,
};
use kplat::{
    boot::BootHandler,
    memory::{p2v, pa},
};
use log::*;

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
    fn early_init(_cpu_id: usize, dtb: usize) {
        map_kvm_guarded_mmio();
        crate::mem::early_init(dtb);
        aarch64_peripherals::ns16550a::early_init(p2v(pa!(UART_PADDR)));
        aarch64_peripherals::psci::init(PSCI_METHOD);
        aarch64_peripherals::generic_timer::early_init();
        #[cfg(feature = "rtc")]
        aarch64_peripherals::pl031::early_init(p2v(pa!(RTC_PADDR)));
    }

    #[cfg(feature = "smp")]
    fn early_init_ap(_cpu_id: usize) {}

    /// Finish platform init after core subsystems are online.
    fn final_init(cpu_id: usize, dtb: usize) {
        info!("cpu_id {}", cpu_id);
        crate::fdt::init_fdt(p2v(pa!(dtb)));
        crate::gicv3::init_gic(p2v(pa!(GICD_PADDR)), p2v(pa!(GICR_PADDR)));
        info!("set UART IRQ {} as edge trigger", UART_IRQ);
        crate::gicv3::set_trigger(UART_IRQ, true);
        aarch64_peripherals::generic_timer::enable_local(TIMER_IRQ);
    }

    #[cfg(feature = "smp")]
    /// Finalize per-CPU setup on secondary cores.
    fn final_init_ap(_cpu_id: usize) {
        crate::gicv3::init_gic(p2v(pa!(GICD_PADDR)), p2v(pa!(GICR_PADDR)));
        aarch64_peripherals::generic_timer::enable_local(TIMER_IRQ);
    }
}

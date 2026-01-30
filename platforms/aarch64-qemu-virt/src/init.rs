//! Platform initialization hooks for aarch64-qemu-virt.

use kplat::{
    boot::BootHandler,
    memory::{p2v, pa},
};

#[allow(unused_imports)]
use crate::config::devices::{GICC_PADDR, GICD_PADDR, RTC_PADDR, TIMER_IRQ, UART_IRQ, UART_PADDR};
use crate::config::plat::PSCI_METHOD;
struct BootHandlerImpl;
#[impl_dev_interface]
impl BootHandler for BootHandlerImpl {
    fn early_init(_cpu_id: usize, _dtb: usize) {
        kcpu::boot::init_trap();
        aarch64_peripherals::pl011::early_init(p2v(pa!(UART_PADDR)));
        aarch64_peripherals::psci::init(PSCI_METHOD);
        aarch64_peripherals::generic_timer::early_init();
        #[cfg(feature = "rtc")]
        aarch64_peripherals::pl031::early_init(p2v(pa!(RTC_PADDR)));
    }

    #[cfg(feature = "smp")]
    fn early_init_ap(_cpu_id: usize) {
        kcpu::boot::init_trap();
    }

    fn final_init(_cpu_id: usize, _dtb: usize) {
        aarch64_peripherals::gic::init_gic(p2v(pa!(GICD_PADDR)), p2v(pa!(GICC_PADDR)));
        aarch64_peripherals::gic::init_gicc();
        aarch64_peripherals::generic_timer::enable_local(TIMER_IRQ);
    }

    #[cfg(feature = "smp")]
    fn final_init_ap(_cpu_id: usize) {
        aarch64_peripherals::gic::init_gicc();
        aarch64_peripherals::generic_timer::enable_local(TIMER_IRQ);
    }
}

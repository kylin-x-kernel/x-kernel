//! Raspberry Pi boot initialization hooks.
use kplat::boot::BootHandler;
use kplat::memory::{pa, p2v};
#[allow(unused_imports)]
use crate::config::devices::{GICC_PADDR, GICD_PADDR, TIMER_IRQ, UART_IRQ, UART_PADDR};
struct BootHandlerImpl;
#[impl_dev_interface]
impl BootHandler for BootHandlerImpl {
    fn early_init(_cpu_id: usize, _dtb: usize) {
        kcpu::boot::init_trap();
        kplat_aarch64_peripherals::pl011::early_init(p2v(pa!(UART_PADDR)));
        kplat_aarch64_peripherals::generic_timer::early_init();
    }
    #[cfg(feature = "smp")]
    fn early_init_secondary(_cpu_id: usize) {
        kcpu::boot::init_trap();
    }
    fn final_init(_cpu_id: usize, _dtb: usize) {
        #[cfg(feature = "irq")]
        {
            kplat_aarch64_peripherals::gic::init_gic(
                p2v(pa!(GICD_PADDR)),
                p2v(pa!(GICC_PADDR)),
            );
            kplat_aarch64_peripherals::gic::init_gicc();
            kplat_aarch64_peripherals::generic_timer::enable_local(TIMER_IRQ);
        }
    }
    #[cfg(feature = "smp")]
    fn final_init_secondary(_cpu_id: usize) {
        #[cfg(feature = "irq")]
        {
            kplat_aarch64_peripherals::gic::init_gicc();
            kplat_aarch64_peripherals::generic_timer::enable_local(TIMER_IRQ);
        }
    }
}

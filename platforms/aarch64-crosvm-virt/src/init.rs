use kplat::{
    init::BootHandler,
    mem::{p2v, pa},
};
use log::*;

#[allow(unused_imports)]
use crate::config::devices::{GICD_PADDR, GICR_PADDR, RTC_PADDR, TIMER_IRQ, UART_IRQ, UART_PADDR};
use crate::{config::plat::PSCI_METHOD, serial::*};
struct BootHandlerImpl;
#[impl_dev_interface]
impl BootHandler for BootHandlerImpl {
    fn early_init(_cpu_id: usize, dtb: usize) {
        boot_print_str("[boot] platform init early\r\n");
        crate::mem::early_init(dtb);
        axcpu::init::init_trap();
        kplat_aarch64_peripherals::ns16550a::early_init(p2v(pa!(UART_PADDR)));
        kplat_aarch64_peripherals::psci::init(PSCI_METHOD);
        kplat_aarch64_peripherals::generic_timer::early_init();
        #[cfg(feature = "rtc")]
        kplat_aarch64_peripherals::pl031::early_init(p2v(pa!(RTC_PADDR)));
    }

    #[cfg(feature = "smp")]
    fn early_init_secondary(_cpu_id: usize) {
        axcpu::init::init_trap();
    }

    fn final_init(cpu_id: usize, dtb: usize) {
        info!("cpu_id {}", cpu_id);
        crate::fdt::init_fdt(p2v(pa!(dtb)));
        #[cfg(feature = "irq")]
        {
            crate::gicv3::init_gic(p2v(pa!(GICD_PADDR)), p2v(pa!(GICR_PADDR)));
            info!("set UART IRQ {} as edge trigger", UART_IRQ);
            crate::gicv3::set_trigger(UART_IRQ, true);
            kplat_aarch64_peripherals::generic_timer::enable_local(TIMER_IRQ);
        }
    }

    #[cfg(feature = "smp")]
    fn final_init_secondary(_cpu_id: usize) {
        #[cfg(feature = "irq")]
        {
            crate::gicv3::init_gic(p2v(pa!(GICD_PADDR)), p2v(pa!(GICR_PADDR)));
            kplat_aarch64_peripherals::generic_timer::enable_local(TIMER_IRQ);
        }
    }
}

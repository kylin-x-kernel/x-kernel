use kplat::{
    mem::{p2v, pa},
    power::SysCtrl,
};

use crate::config::devices::GED_PADDR;
struct PowerImpl;
#[impl_dev_interface]
impl SysCtrl for PowerImpl {
    #[cfg(feature = "smp")]
    fn boot_ap(cpu_id: usize, stack_top_paddr: usize) {
        crate::mp::start_secondary_cpu(cpu_id, pa!(stack_top_paddr));
    }

    fn shutdown() -> ! {
        let halt_addr = p2v(pa!(GED_PADDR)).as_mut_ptr();
        info!("Shutting down...");
        unsafe { halt_addr.write_volatile(0x34) };
        axcpu::asm::halt();
        warn!("It should shutdown!");
        loop {
            axcpu::asm::halt();
        }
    }
}

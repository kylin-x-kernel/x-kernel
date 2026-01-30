//! Raspberry Pi system control implementation.
use kplat::sys::SysCtrl;
struct PowerImpl;
#[impl_dev_interface]
impl SysCtrl for PowerImpl {
    #[cfg(feature = "smp")]
    fn boot_ap(cpu_id: usize, stack_top_paddr: usize) {
        crate::mp::start_secondary_cpu(cpu_id, kplat::memory::pa!(stack_top_paddr));
    }
    fn shutdown() -> ! {
        log::info!("Shutting down...");
        loop {
            kcpu::instrs::stop_cpu();
        }
    }
}

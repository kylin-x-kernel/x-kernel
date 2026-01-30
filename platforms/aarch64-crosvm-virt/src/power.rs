//! Power and SMP boot controls for the platform.
use kplat::sys::SysCtrl;
struct PowerImpl;
#[impl_dev_interface]
impl SysCtrl for PowerImpl {
    /// Power on an application processor (AP) with a provided stack.
    #[cfg(feature = "smp")]
    fn boot_ap(cpu_id: usize, stack_top_paddr: usize) {
        use kplat::memory::{v2p, va};
        let entry_paddr = v2p(va!(crate::boot::_start_secondary as *const () as usize));
        aarch64_peripherals::psci::cpu_on(cpu_id, entry_paddr.as_usize(), stack_top_paddr);
    }

    /// Request a system shutdown through PSCI.
    fn shutdown() -> ! {
        aarch64_peripherals::psci::shutdown()
    }
}

//! Power control implementation for aarch64-qemu-virt.

use kplat::sys::SysCtrl;
struct PowerImpl;
#[impl_dev_interface]
impl SysCtrl for PowerImpl {
    #[cfg(feature = "smp")]
    fn boot_ap(cpu_id: usize, stack_top_paddr: usize) {
        use kplat::memory::{v2p, va};
        let entry_paddr = v2p(va!(crate::boot::_start_secondary as *const () as usize));
        aarch64_peripherals::psci::cpu_on(cpu_id, entry_paddr.as_usize(), stack_top_paddr);
    }

    fn shutdown() -> ! {
        aarch64_peripherals::psci::shutdown()
    }
}

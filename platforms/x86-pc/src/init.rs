use kplat::boot::BootHandler;
struct BootHandlerImpl;
#[impl_dev_interface]
impl BootHandler for BootHandlerImpl {
    fn early_init(_cpu_id: usize, mbi: usize) {
        axcpu::init::init_trap();
        crate::console::init();
        crate::time::early_init();
        crate::mem::init(mbi);
    }

    #[cfg(feature = "smp")]
    fn early_init_secondary(_cpu_id: usize) {
        axcpu::init::init_trap();
    }

    fn final_init(_cpu_id: usize, _arg: usize) {
        crate::apic::init_primary();
        crate::time::init_primary();
    }

    #[cfg(feature = "smp")]
    fn final_init_secondary(_cpu_id: usize) {
        crate::apic::init_secondary();
        crate::time::init_secondary();
    }
}

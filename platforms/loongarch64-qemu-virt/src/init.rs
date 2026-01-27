use kplat::boot::BootHandler;
struct BootHandlerImpl;
#[impl_dev_interface]
impl BootHandler for BootHandlerImpl {
    fn early_init(_cpu_id: usize, _mbi: usize) {
        axcpu::init::init_trap();
        crate::console::early_init();
        crate::time::early_init();
    }

    #[cfg(feature = "smp")]
    fn early_init_secondary(_cpu_id: usize) {
        axcpu::init::init_trap();
    }

    fn final_init(_cpu_id: usize, _arg: usize) {
        #[cfg(feature = "irq")]
        crate::irq::init();
        crate::time::init_percpu();
    }

    #[cfg(feature = "smp")]
    fn final_init_secondary(_cpu_id: usize) {
        crate::time::init_percpu();
    }
}

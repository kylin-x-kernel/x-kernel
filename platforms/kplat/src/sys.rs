use kplat_macros::device_interface;

#[device_interface]
pub trait SysCtrl {
    #[cfg(feature = "smp")]
    fn boot_ap(id: usize, stack_top: usize);

    fn shutdown() -> !;
}

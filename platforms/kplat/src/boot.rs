use kplat_macros::device_interface;

#[device_interface]
pub trait BootHandler {
    fn early_init(id: usize, dtb: usize);

    #[cfg(feature = "smp")]
    fn early_init_ap(id: usize);

    fn final_init(id: usize, dtb: usize);

    #[cfg(feature = "smp")]
    fn final_init_ap(id: usize);
}

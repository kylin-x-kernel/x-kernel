//! Platform boot-stage interface definitions.

use kplat_macros::device_interface;

#[device_interface]
pub trait BootHandler {
    /// Early initialization on the boot CPU.
    fn early_init(id: usize, dtb: usize);

    #[cfg(feature = "smp")]
    /// Early initialization on an application processor (SMP only).
    fn early_init_ap(id: usize);

    /// Final initialization on the boot CPU.
    fn final_init(id: usize, dtb: usize);

    #[cfg(feature = "smp")]
    /// Final initialization on an application processor (SMP only).
    fn final_init_ap(id: usize);
}

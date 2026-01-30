//! Platform system control interface.

use kplat_macros::device_interface;

#[device_interface]
pub trait SysCtrl {
    #[cfg(feature = "smp")]
    /// Boots an application processor.
    fn boot_ap(id: usize, stack_top: usize);

    /// Shuts down the system.
    fn shutdown() -> !;
}

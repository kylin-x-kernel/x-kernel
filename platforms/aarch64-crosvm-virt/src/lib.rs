//! Platform support for the aarch64 crosvm-virt target.
#![no_std]
#[macro_use]
extern crate kplat;
mod boot;
pub mod fdt;
mod gicv3;
mod init;
mod mem;
mod power;
pub mod psci;
mod serial;
pub mod config {
    platconfig_macros::include_configs!(path_env = "PLAT_CONFIG_PATH", fallback = "axconfig.toml");
    check_str_eq!(
        PACKAGE,
        env!("CARGO_PKG_NAME"),
        "`PACKAGE` field in the configuration does not match the Package name. Please check your \
         configuration file."
    );
}
aarch64_peripherals::ns16550_console_if_impl!(ConsoleImpl);
aarch64_peripherals::time_if_impl!(GlobalTimerImpl);
irq_if_impl!(IntrManagerImpl);

//! Raspberry Pi (AArch64) platform support.
#![no_std]
#[macro_use]
extern crate kplat;
mod boot;
mod init;
mod mem;
mod power;
#[cfg(feature = "smp")]
mod mp;
pub mod config {
    platconfig_macros::include_configs!(path_env = "PLAT_CONFIG_PATH", fallback = "axconfig.toml");
    assert_str_eq!(
        PACKAGE,
        env!("CARGO_PKG_NAME"),
        "`PACKAGE` field in the configuration does not match the Package name. Please check your configuration file."
    );
}
kplat_aarch64_peripherals::console_if_impl!(ConsoleImpl);
kplat_aarch64_peripherals::time_if_impl!(GlobalTimerImpl);
#[cfg(feature = "irq")]
kplat_aarch64_peripherals::irq_if_impl!(IntrManagerImpl);

//! Platform support for aarch64-qemu-virt.

#![no_std]
#[macro_use]
extern crate kplat;
mod boot;
mod init;
mod mem;
mod power;
pub mod config {
    platconfig_macros::include_configs!(path_env = "PLAT_CONFIG_PATH", fallback = "axconfig.toml");
    // assert_eq!(
    // PACKAGE,
    // env!("CARGO_PKG_NAME"),
    // "`PACKAGE` field in the configuration does not match the Package name. Please check your \
    // configuration file."
    // );
}
aarch64_peripherals::console_if_impl!(ConsoleImpl);
aarch64_peripherals::time_if_impl!(GlobalTimerImpl);
aarch64_peripherals::irq_if_impl!(IntrManagerImpl);
#[cfg(feature = "pmu")]
aarch64_peripherals::pmu_if_impl!(PerfMgrImpl);
#[cfg(feature = "nmi")]
aarch64_peripherals::nmi_if_impl!(NmiIfImpl);

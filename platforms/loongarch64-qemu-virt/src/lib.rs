#![cfg(target_arch = "loongarch64")]
#![no_std]
#[macro_use]
extern crate log;
#[macro_use]
extern crate kplat;
pub mod config {
    platconfig_macros::include_configs!(path_env = "PLAT_CONFIG_PATH", fallback = "axconfig.toml");
    assert_str_eq!(
        PACKAGE,
        env!("CARGO_PKG_NAME"),
        "`PACKAGE` field in the configuration does not match the Package name. Please check your \
         configuration file."
    );
}
mod boot;
mod console;
mod init;
mod irq;
mod mem;
#[cfg(feature = "smp")]
mod mp;
mod power;
mod time;

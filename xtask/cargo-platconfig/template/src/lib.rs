#![no_std]

#[macro_use]
extern crate kplat;

mod console;
mod init;
#[cfg(feature = "irq")]
mod irq;
mod mem;
mod power;
mod time;

pub mod config {
    //! Platform configuration module.
    //!
    //! If the `PLAT_CONFIG_PATH` environment variable is set, it will load the configuration from the specified path.
    //! Otherwise, it will fall back to the `platconfig.toml` file in the current directory and generate the default configuration.
    //!
    //! If the `PACKAGE` field in the configuration does not match the package name, it will panic with an error message.
    platconfig_macros::include_configs!(path_env = "PLAT_CONFIG_PATH", fallback = "platconfig.toml");
    assert_str_eq!(
        PACKAGE,
        env!("CARGO_PKG_NAME"),
        "`PACKAGE` field in the configuration does not match the Package name. Please check your configuration file."
    );
}

#[unsafe(no_mangle)]
unsafe extern "C" fn _start() -> ! {
    // TODO: Implement actual bootstrap logic
    kplat::call_main(0, 0);
}

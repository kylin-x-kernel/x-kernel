#![no_std]

#[macro_use]
extern crate log;

pub use kdriver::prelude::DisplayInfo;
use kdriver::{DeviceContainer, prelude::*};
use ksync::Mutex;
use lazyinit::LazyInit;

static PRIMARY_FB: LazyInit<Mutex<DisplayDevice>> = LazyInit::new();

pub fn fb_init(mut display_devs: DeviceContainer<DisplayDevice>) {
    info!("Initialize framebuffer subsystem...");

    if let Some(dev) = display_devs.take_one() {
        info!("  use framebuffer device 0: {:?}", dev.name());
        PRIMARY_FB.init_once(Mutex::new(dev));
    } else {
        warn!("  No framebuffer device found!");
    }
}

pub fn fb_available() -> bool {
    PRIMARY_FB.is_inited()
}

pub fn fb_info() -> DisplayInfo {
    PRIMARY_FB.lock().info()
}

pub fn fb_flush() -> bool {
    PRIMARY_FB.lock().flush().is_ok()
}

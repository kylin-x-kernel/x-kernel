#![no_std]

#[macro_use]
extern crate log;

pub use axdriver::prelude::DisplayInfo;
use axdriver::{AxDeviceContainer, prelude::*};
use axsync::Mutex;
use lazyinit::LazyInit;

static PRIMARY_FB: LazyInit<Mutex<AxDisplayDevice>> = LazyInit::new();

pub fn fb_init(mut display_devs: AxDeviceContainer<AxDisplayDevice>) {
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

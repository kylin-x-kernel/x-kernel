//! Input device initialization and access helpers.
#![no_std]

#[macro_use]
extern crate log;
extern crate alloc;

use alloc::vec::Vec;
use core::mem;

use kdriver::{DeviceContainer, prelude::*};
use ksync::Mutex;
use lazyinit::LazyInit;

static DEVICES: LazyInit<Mutex<Vec<InputDevice>>> = LazyInit::new();

/// Initialize the input subsystem with detected devices.
pub fn init_input(mut input_devs: DeviceContainer<InputDevice>) {
    info!("Initialize input subsystem...");

    let mut devices = Vec::new();
    while let Some(dev) = input_devs.take_one() {
        info!(
            "  registered a new {:?} input device: {}",
            dev.device_kind(),
            dev.name(),
        );
        devices.push(dev);
    }
    DEVICES.init_once(Mutex::new(devices));
}

/// Take ownership of all registered input devices.
pub fn input_take_all() -> Vec<InputDevice> {
    mem::take(&mut DEVICES.lock())
}

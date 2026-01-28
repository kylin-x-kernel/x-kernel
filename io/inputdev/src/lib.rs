#![no_std]

#[macro_use]
extern crate log;
extern crate alloc;

use alloc::vec::Vec;
use core::mem;

use axdriver::{AxDeviceContainer, prelude::*};
use axsync::Mutex;
use lazyinit::LazyInit;

static INPUT_DEVICES: LazyInit<Mutex<Vec<AxInputDevice>>> = LazyInit::new();

pub fn input_init(mut input_devs: AxDeviceContainer<AxInputDevice>) {
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
    INPUT_DEVICES.init_once(Mutex::new(devices));
}

pub fn input_take_all() -> Vec<AxInputDevice> {
    mem::take(&mut INPUT_DEVICES.lock())
}

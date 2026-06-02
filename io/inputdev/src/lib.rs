// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Input device initialization and access helpers.
#![no_std]

#[macro_use]
extern crate log;
extern crate alloc;

use alloc::{sync::Arc, vec::Vec};

use kclass::{
    ClassDevice, InputDeviceImpl, input_devices as class_input_devices, subscribe_input_available,
};
use kdevice::subscribe_device_removed;
use ksync::Mutex;
use lazyinit::LazyInit;

static INPUT_DEVICES: LazyInit<Mutex<Vec<ClassDevice<InputDeviceImpl>>>> = LazyInit::new();

/// Initialize the input subsystem with detected devices.
pub fn init_input() {
    info!("Initialize input subsystem...");

    INPUT_DEVICES.init_once(Mutex::new(Vec::new()));

    register_input_devices(class_input_devices());
    subscribe_input_available(Arc::new(register_input_device));

    subscribe_input_unregister();
}

fn register_input_devices(devices: Vec<ClassDevice<InputDeviceImpl>>) {
    for handle in devices {
        register_input_device(handle);
    }
}

fn register_input_device(handle: ClassDevice<InputDeviceImpl>) {
    if !INPUT_DEVICES.is_inited() {
        return;
    }

    let mut devices = INPUT_DEVICES.lock();
    if devices.iter().any(|device| device.id() == handle.id()) {
        return;
    }

    info!(
        "  input device activated: {:?} (driver={}, {:?})",
        handle.name(),
        handle.driver_name(),
        handle.location(),
    );
    devices.push(handle);
}

fn subscribe_input_unregister() {
    subscribe_device_removed(Arc::new(|id| {
        if !INPUT_DEVICES.is_inited() {
            return;
        }
        let mut devs = INPUT_DEVICES.lock();
        if let Some(pos) = devs.iter().position(|h| h.id() == id) {
            devs.swap_remove(pos);
            info!("input: unregistered removed device {:?}", id);
        }
    }));
}

/// Drain all registered input device handles out of the input subsystem.
pub fn input_drain_devices() -> Vec<ClassDevice<InputDeviceImpl>> {
    INPUT_DEVICES.lock().drain(..).collect()
}

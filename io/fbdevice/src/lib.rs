// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Framebuffer device initialization and access helpers.
#![no_std]

#[macro_use]
extern crate log;

extern crate alloc;

use alloc::{sync::Arc, vec::Vec};

pub use kclass::DisplayInfo;
use kclass::{
    ClassDevice, DisplayDevice, DisplayDeviceImpl, display_devices as class_display_devices,
    subscribe_display_available,
};
use kdevice::subscribe_device_removed;
use ksync::Mutex;
use lazyinit::LazyInit;

static DISPLAY_DEVICES: LazyInit<Mutex<Vec<ClassDevice<DisplayDeviceImpl>>>> = LazyInit::new();

/// Initialize the framebuffer subsystem with available devices.
pub fn fb_init() {
    info!("Initialize framebuffer subsystem...");

    DISPLAY_DEVICES.init_once(Mutex::new(Vec::new()));

    register_display_devices(class_display_devices());
    subscribe_display_available(Arc::new(register_display_device));

    subscribe_display_unregister();
}

fn register_display_devices(devices: Vec<ClassDevice<DisplayDeviceImpl>>) {
    for handle in devices {
        register_display_device(handle);
    }
}

fn register_display_device(handle: ClassDevice<DisplayDeviceImpl>) {
    if !DISPLAY_DEVICES.is_inited() {
        return;
    }

    let mut devices = DISPLAY_DEVICES.lock();
    if devices.iter().any(|device| device.id() == handle.id()) {
        return;
    }

    info!(
        "  display device activated: {:?} (driver={}, {:?})",
        handle.name(),
        handle.driver_name(),
        handle.location(),
    );
    devices.push(handle);
}

fn subscribe_display_unregister() {
    subscribe_device_removed(Arc::new(|id| {
        if !DISPLAY_DEVICES.is_inited() {
            return;
        }
        let mut devs = DISPLAY_DEVICES.lock();
        if let Some(pos) = devs.iter().position(|h| h.id() == id) {
            devs.swap_remove(pos);
            info!("display: unregistered removed device {:?}", id);
        }
    }));
}

/// Returns whether a primary framebuffer is available.
pub fn fb_available() -> bool {
    DISPLAY_DEVICES.is_inited() && !DISPLAY_DEVICES.lock().is_empty()
}

/// Returns display information for the primary framebuffer.
pub fn fb_info() -> DisplayInfo {
    DISPLAY_DEVICES
        .lock()
        .first_mut()
        .expect("no display device")
        .info()
}

/// Flush the primary framebuffer to the display.
pub fn fb_flush() -> bool {
    DISPLAY_DEVICES
        .lock()
        .first_mut()
        .expect("no display device")
        .flush()
        .is_ok()
}

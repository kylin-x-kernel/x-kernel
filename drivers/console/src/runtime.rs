// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Runtime console subsystem.
//!
//! The boot console remains a boot-time special path. Once the already active
//! boot console is adopted into the driver core, its runtime wrapper is
//! published through the generic char class. This module keeps a filtered local
//! console view so consumers can use an explicit active/default console
//! instead of scanning every character device.

use alloc::{sync::Arc, vec::Vec};

use driver_base::{DriverError, DriverResult};
use kclass::{
    CharDevice, CharDeviceImpl, ClassDevice, char_devices, publish_char, subscribe_char_available,
};
use kdevice::{DeviceId, DeviceObject, subscribe_device_removed};
use kspin::SpinNoPreempt;
use lazyinit::LazyInit;

struct ConsoleSubsystem {
    devices: Vec<ClassDevice<CharDeviceImpl>>,
    default: Option<DeviceId>,
    active: Option<DeviceId>,
}

impl ConsoleSubsystem {
    fn new() -> Self {
        Self {
            devices: Vec::new(),
            default: None,
            active: None,
        }
    }

    fn add(&mut self, handle: ClassDevice<CharDeviceImpl>) {
        let id = handle.id();
        if self.devices.iter().any(|current| current.id() == id) {
            return;
        }
        if self.default.is_none() {
            self.default = Some(id);
        }
        if self.active.is_none() {
            self.active = Some(id);
        }
        log::debug!(
            "console subsystem: added {:?} (driver={})",
            id,
            handle.driver_name()
        );
        self.devices.push(handle);
    }

    fn remove(&mut self, id: DeviceId) -> Option<ClassDevice<CharDeviceImpl>> {
        let removed = self
            .devices
            .iter()
            .position(|device| device.id() == id)
            .map(|pos| self.devices.swap_remove(pos));
        if removed.is_some() {
            if self.default == Some(id) {
                self.default = self.devices.first().map(ClassDevice::id);
            }
            if self.active == Some(id) {
                self.active = self
                    .default
                    .filter(|default| self.devices.iter().any(|device| device.id() == *default))
                    .or_else(|| self.devices.first().map(ClassDevice::id));
            }
        }
        removed
    }

    fn set_active(&mut self, id: DeviceId) -> DriverResult<()> {
        if self.devices.iter().any(|device| device.id() == id) {
            self.active = Some(id);
            Ok(())
        } else {
            Err(DriverError::InvalidInput)
        }
    }

    fn active_id(&self) -> Option<DeviceId> {
        self.active
    }

    fn len(&self) -> usize {
        self.devices.len()
    }

    /// Snapshot a clone of the currently active console handle.
    ///
    /// `ClassDevice<T>` wraps an `Arc<Inner<T>>`, so cloning is cheap and
    /// the caller can drop the subsystem lock before doing IO. This keeps
    /// the subsystem critical section short and avoids holding it across
    /// per-device IO, which itself takes the inner spinlock via
    /// `with_mut(...)`.
    fn active_handle(&self) -> Option<ClassDevice<CharDeviceImpl>> {
        let id = self.active?;
        self.devices
            .iter()
            .find(|device| device.id() == id)
            .cloned()
    }
}

static CONSOLE_SUBSYS: LazyInit<SpinNoPreempt<ConsoleSubsystem>> = LazyInit::new();
static CONSOLE_CLASS_BRIDGE: LazyInit<()> = LazyInit::new();

fn console_subsystem() -> &'static SpinNoPreempt<ConsoleSubsystem> {
    CONSOLE_SUBSYS.call_once(|| SpinNoPreempt::new(ConsoleSubsystem::new()));
    CONSOLE_SUBSYS
        .get()
        .expect("console subsystem call_once succeeded")
}

fn is_runtime_console(handle: &ClassDevice<CharDeviceImpl>) -> bool {
    handle.driver_name() == "console"
}

fn add_console_device(handle: ClassDevice<CharDeviceImpl>) {
    if !is_runtime_console(&handle) {
        return;
    }
    console_subsystem().lock().add(handle);
}

fn ensure_console_bridge() {
    CONSOLE_CLASS_BRIDGE.call_once(|| {
        for handle in char_devices() {
            add_console_device(handle);
        }
        subscribe_char_available(Arc::new(add_console_device));
        subscribe_device_removed(Arc::new(|id| {
            let _ = unregister_console_device(id);
        }));
    });
}

/// Publish a runtime console instance and mirror it into the local console view.
pub fn register_console_runtime(
    parent: Arc<DeviceObject>,
    runtime: CharDeviceImpl,
) -> DriverResult<()> {
    ensure_console_bridge();
    publish_char(parent, runtime)
}

/// Remove a runtime console from the console subsystem.
pub fn unregister_console_device(id: DeviceId) -> Option<ClassDevice<CharDeviceImpl>> {
    console_subsystem().lock().remove(id)
}

/// Select the active runtime console.
pub fn set_active_console(id: DeviceId) -> DriverResult<()> {
    console_subsystem().lock().set_active(id)
}

/// Current active runtime console ID.
pub fn active_console_id() -> Option<DeviceId> {
    console_subsystem().lock().active_id()
}

/// Count runtime console devices registered with the console subsystem.
pub fn console_device_count() -> usize {
    console_subsystem().lock().len()
}

/// Read from the active runtime console, if one exists.
pub fn read_active_console(buf: &mut [u8]) -> Option<DriverResult<usize>> {
    // Snapshot the active console handle under the subsystem lock, then drop
    // the lock before doing IO. The character device's own lock is taken
    // inside `read(...)`; nesting subsystem -> per-device lock here is what
    // we want to avoid.
    let mut handle = console_subsystem().lock().active_handle()?;
    Some(handle.read(buf))
}

/// Write to the active runtime console, if one exists.
pub fn write_active_console(buf: &[u8]) -> Option<DriverResult<usize>> {
    let mut handle = console_subsystem().lock().active_handle()?;
    Some(handle.write(buf))
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Runtime console subsystem.
//!
//! The boot console remains a boot-time special path. Once the already active
//! boot console is adopted into the driver core, its runtime wrapper is
//! published through the generic char class. This module keeps a filtered local
//! console view and the adopted stdout identity, without a public console
//! selection mechanism.

use alloc::{sync::Arc, vec::Vec};

use driver_base::DriverResult;
use kclass::{
    CharDeviceImpl, ClassDevice, char_devices, find_char_device, publish_char,
    subscribe_char_available,
};
use kdevice::{DeviceId, DeviceObject, subscribe_device_removed};
use kspin::SpinNoPreempt;
use lazyinit::LazyInit;

struct ConsoleSubsystem {
    devices: Vec<ClassDevice<CharDeviceImpl>>,
    stdout: Option<DeviceId>,
}

impl ConsoleSubsystem {
    fn new() -> Self {
        Self {
            devices: Vec::new(),
            stdout: None,
        }
    }

    fn add(&mut self, handle: ClassDevice<CharDeviceImpl>) {
        let id = handle.id();
        if self.devices.iter().any(|current| current.id() == id) {
            return;
        }
        log::debug!(
            "console subsystem: added {:?} (driver={})",
            id,
            handle.driver_name()
        );
        self.devices.push(handle);
    }

    fn remove(&mut self, id: DeviceId) -> Option<ClassDevice<CharDeviceImpl>> {
        if self.stdout == Some(id) {
            self.stdout = None;
        }
        self.devices
            .iter()
            .position(|device| device.id() == id)
            .map(|pos| self.devices.swap_remove(pos))
    }

    fn stdout_handle(&self) -> Option<ClassDevice<CharDeviceImpl>> {
        let id = self.stdout?;
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
    // Serial stdout is matched separately by its registered device ID, never
    // by driver name: auxiliary UARTs use the same serial driver.
    handle.driver_name() == "console"
}

fn add_console_device(handle: ClassDevice<CharDeviceImpl>) {
    let is_console = is_runtime_console(&handle);
    let mut subsystem = console_subsystem().lock();
    if !is_console && subsystem.stdout != Some(handle.id()) {
        return;
    }
    subsystem.add(handle);
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

/// Publish the adopted boot stdout device and remember its runtime identity.
///
/// Only the caller's device is adopted as a console — the serial driver uses
/// this for its stdout port. Auxiliary serial ports are published via
/// `publish_char` and ignored by the char-availability bridge, so they
/// cannot replace the stdout association.
/// The runtime implementation must wrap the same early stdout port, not another
/// device. Called by serial adoption in task context, before driver activation.
pub fn register_stdout_console(
    parent: Arc<DeviceObject>,
    runtime: CharDeviceImpl,
) -> DriverResult<()> {
    ensure_console_bridge();
    let id = parent.id();
    publish_char(parent, runtime)?;
    console_subsystem().lock().stdout = Some(id);
    // This runs inside the driver's `probe_device`, so the device is usually
    // not active yet and `find_char_device()` (which filters on availability)
    // cannot see it. Adopt immediately when already active; otherwise remember
    // the id and adopt when the device activates.
    if let Some(handle) = find_char_device(id) {
        console_subsystem().lock().add(handle);
        return Ok(());
    }
    Ok(())
}

/// Snapshot the available runtime device associated with boot stdout.
///
/// Task context. Returns none until the device is published and active. The
/// handle retains ownership, not availability: callers must check availability
/// before later I/O. This does not provide synchronization for hot-unplug.
pub fn stdout_console() -> Option<ClassDevice<CharDeviceImpl>> {
    let handle = console_subsystem().lock().stdout_handle()?;
    handle.is_available().then_some(handle)
}

/// Identity of the adopted boot stdout.
///
/// Task context. Available after publication, including before activation;
/// cleared on removal. This identifies the port, not its I/O availability.
pub fn stdout_console_id() -> Option<DeviceId> {
    console_subsystem().lock().stdout
}

/// Remove a runtime console from the console subsystem.
pub fn unregister_console_device(id: DeviceId) -> Option<ClassDevice<CharDeviceImpl>> {
    console_subsystem().lock().remove(id)
}

#[cfg(unittest)]
#[path = "tests/runtime.rs"]
mod tests;

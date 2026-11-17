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
    CharDeviceImpl, ClassDevice, char_devices, publish_char, subscribe_char_available,
    subscribe_char_removed,
};
use kdevice::PublicationId;
use kspin::SpinNoPreempt;
use lazyinit::LazyInit;

struct ConsoleSubsystem {
    devices: Vec<ClassDevice<CharDeviceImpl>>,
    stdout: Option<PublicationId>,
}

impl ConsoleSubsystem {
    fn new() -> Self {
        Self {
            devices: Vec::new(),
            stdout: None,
        }
    }

    fn add(&mut self, handle: ClassDevice<CharDeviceImpl>) {
        let publication = handle.publication_id();
        if self
            .devices
            .iter()
            .any(|current| current.publication_id() == publication)
        {
            return;
        }
        log::debug!(
            "console subsystem: added {:?}/{} (driver={})",
            handle.id(),
            publication.raw(),
            handle.driver_name()
        );
        self.devices.push(handle);
    }

    fn remove(&mut self, publication: PublicationId) -> Option<ClassDevice<CharDeviceImpl>> {
        if self.stdout == Some(publication) {
            self.stdout = None;
        }
        self.devices
            .iter()
            .position(|device| device.publication_id() == publication)
            .map(|pos| self.devices.swap_remove(pos))
    }

    fn stdout_handle(&self) -> Option<ClassDevice<CharDeviceImpl>> {
        let publication = self.stdout?;
        self.devices
            .iter()
            .find(|device| device.publication_id() == publication)
            .cloned()
    }
}

static CONSOLE_SUBSYS: LazyInit<SpinNoPreempt<ConsoleSubsystem>> = LazyInit::new();
static CONSOLE_CLASS_BRIDGE: LazyInit<()> = LazyInit::new();

fn console_subsystem() -> &'static SpinNoPreempt<ConsoleSubsystem> {
    CONSOLE_SUBSYS.call_once(|| SpinNoPreempt::new(ConsoleSubsystem::new()));
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: call_once completed successfully and \
                  permanently published the console subsystem"
    )]
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
    if !is_console && subsystem.stdout != Some(handle.publication_id()) {
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
        subscribe_char_removed(Arc::new(|_, publication| {
            let _ = unregister_console_device(publication);
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
    ctx: &kdevice::ProbeContext<'_>,
    runtime: CharDeviceImpl,
) -> DriverResult<()> {
    ensure_console_bridge();
    let publication = publish_char(ctx, runtime)?;
    let publication_id = publication.publication_id();
    console_subsystem().lock().stdout = Some(publication_id);
    // Probe is usually not active yet; if it is, adopt immediately. Activation
    // otherwise reaches the char available callback and matches publication_id.
    // Keep the adopted stdout handle even before its class activation; the
    // publication identity is already fixed and stdout_console() separately
    // checks the captured binding.
    console_subsystem().lock().add(publication.device());
    Ok(())
}

/// Snapshot the available runtime device associated with boot stdout.
///
/// Task context. Returns none until the device is published and active. The
/// handle retains ownership, not availability: callers must check availability
/// before later I/O. This does not provide synchronization for hot-unplug.
fn stdout_handle_is_available(handle: &ClassDevice<CharDeviceImpl>) -> bool {
    handle.is_available()
}

pub fn stdout_console() -> Option<ClassDevice<CharDeviceImpl>> {
    let handle = console_subsystem().lock().stdout_handle();
    let Some(handle) = handle else {
        #[cfg(unittest)]
        log_stdout_missing();
        return None;
    };
    #[cfg(unittest)]
    log::warn!(
        "stdout probe: publication={} state={} binding={:?} available={}",
        handle.publication_id().raw(),
        handle.publication_state_name(),
        handle.binding_ref().state(),
        handle.is_available(),
    );
    stdout_handle_is_available(&handle).then_some(handle)
}

#[cfg(unittest)]
fn log_stdout_missing() {
    let subsystem = console_subsystem().lock();
    log::warn!(
        "stdout probe: slot publication={:?} local_handles={}",
        subsystem.stdout.map(|publication| publication.raw()),
        subsystem.devices.len(),
    );
}

/// Identity of the adopted boot stdout.
///
/// Task context. Available after publication, including before activation;
/// cleared on removal. This identifies the adopted console publication, not
/// the port's I/O availability.
pub fn stdout_console_id() -> Option<PublicationId> {
    console_subsystem().lock().stdout
}

/// Remove a runtime console publication from the console subsystem.
pub fn unregister_console_device(
    publication: PublicationId,
) -> Option<ClassDevice<CharDeviceImpl>> {
    #[cfg(unittest)]
    log::warn!("stdout remove: publication={}", publication.raw());
    console_subsystem().lock().remove(publication)
}

unittest::path_mod_test!(runtime_unittest, "../ut-tests/runtime_unittest.rs");

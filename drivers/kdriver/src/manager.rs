// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unified device discovery manager.

use driver_base::DriverResult;
use kspin::SpinNoPreempt;
use lazyinit::LazyInit;

use crate::{
    bus::manager::{BusManager, default_bus_manager},
    driver_registry,
    enumeration::EnumerationContext,
};

static DEVICE_MANAGER: LazyInit<DeviceManager> = LazyInit::new();

/// Long-lived device manager that owns bus instances and registered drivers.
pub struct DeviceManager {
    bus_mgr: SpinNoPreempt<BusManager>,
    driver_registrar: driver_registry::DriverRegistrar,
}

impl DeviceManager {
    /// Create a manager with the default bus instances and built-in drivers.
    pub fn new() -> Self {
        let mut manager = Self {
            bus_mgr: SpinNoPreempt::new(default_bus_manager()),
            driver_registrar: driver_registry::DriverRegistrar::new(),
        };
        manager.register_default_drivers();
        manager
    }

    fn register_default_drivers(&mut self) {
        #[cfg(feature = "virtio")]
        driver_registry::virtio::register_all(&mut self.driver_registrar);

        #[cfg(feature = "char")]
        driver_registry::char::register_all(&mut self.driver_registrar);

        driver_registry::register_platform_drivers(&mut self.driver_registrar);
    }

    /// Discover devices on all buses and run driver matching/probing.
    pub fn discover_and_probe(&self) -> EnumerationContext {
        let mut context = EnumerationContext::new();
        let active_before = active_device_count();

        {
            let mut bus_mgr = self.bus_mgr.lock();
            bus_mgr.early_init_all();
            bus_mgr.enumerate_all(&mut context);
        }

        let activated_from_desc = context.probe_pending();

        log::info!(
            "Unified discovery: {} descriptor(s) found on bus",
            context.registered_count()
        );
        for device in context.unclaimed() {
            log::info!(
                "  {:?} at {:?} (origin={:?})",
                device.identity(),
                device.location(),
                device.origin()
            );
        }

        let activated = active_device_count().saturating_sub(active_before);

        log::info!(
            "Unified probe: {} activated ({} from descriptors), {} unmatched",
            activated,
            activated_from_desc,
            context.unclaimed_count()
        );

        context
    }

    /// Re-scan buses and probe newly discovered devices.
    pub fn rescan(&self) -> EnumerationContext {
        let mut context = EnumerationContext::new();
        let active_before = active_device_count();
        {
            let mut bus_mgr = self.bus_mgr.lock();
            bus_mgr.rescan_all(&mut context);
        }
        let activated_from_desc = context.probe_pending();
        let activated = active_device_count().saturating_sub(active_before);
        log::info!(
            "Unified rescan: {} activated ({} from descriptors), {} unmatched",
            activated,
            activated_from_desc,
            context.unclaimed_count()
        );
        context
    }

    /// Quiesce every bus backend so they stop emitting new events.
    ///
    /// Used during system shutdown or suspend prior to draining higher
    /// layers.
    pub fn quiesce_buses(&self) {
        let mut bus_mgr = self.bus_mgr.lock();
        bus_mgr.quiesce_all();
    }

    /// Tear down every bus backend.
    ///
    /// Intended for orderly shutdown. Each backend reports failures via the
    /// log; the manager keeps draining the rest.
    pub fn remove_buses(&self) {
        let mut bus_mgr = self.bus_mgr.lock();
        bus_mgr.remove_all();
    }

    /// Mark a device removed through the driver-core remove path.
    pub fn remove_device(&self, id: kdevice::DeviceId) -> DriverResult {
        self.driver_registrar.remove_device(id)?;
        Ok(())
    }

    /// Access the built-in driver registrar facade.
    pub fn driver_registrar(&self) -> &driver_registry::DriverRegistrar {
        &self.driver_registrar
    }
}

impl Default for DeviceManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Acquire the long-lived device manager.
pub fn device_manager() -> &'static DeviceManager {
    DEVICE_MANAGER.call_once(DeviceManager::new);
    DEVICE_MANAGER
        .get()
        .expect("DEVICE_MANAGER call_once succeeded")
}

/// Run the unified discovery pipeline.
///
/// Enumerates devices on all registered bus backends, then runs the
/// driver match -> bind -> activate flow over discovered devices.
pub fn discover_unified() -> EnumerationContext {
    log::info!("Unified device discovery...");
    device_manager().discover_and_probe()
}

fn active_device_count() -> usize {
    kdevice::device_records_snapshot()
        .into_iter()
        .filter(|record| record.state == kdevice::DeviceState::Active)
        .count()
}

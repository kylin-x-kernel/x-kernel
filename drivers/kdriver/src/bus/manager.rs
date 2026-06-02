// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Bus backend manager for unified device discovery.

use alloc::{boxed::Box, vec::Vec};

use kdevice::BusId;

use super::backend::BusBackend;
use crate::enumeration::EnumerationContext;

/// Manages multiple [`BusBackend`]s that coexist at runtime.
pub struct BusManager {
    backends: Vec<(BusId, Box<dyn BusBackend>)>,
}

impl BusManager {
    /// Create an empty bus manager.
    pub fn new() -> Self {
        Self {
            backends: Vec::new(),
        }
    }

    /// Register a bus backend.
    ///
    /// Assigns a unique [`BusId`] and records the bus in the driver core.
    pub fn register(&mut self, backend: Box<dyn BusBackend>) {
        let bus_type_id = backend.bus_type_id();
        let name = backend.name();
        let bus = kdevice::register_bus_instance(bus_type_id, name);
        let bus_id = bus.id();
        log::info!(
            "bus manager: registered backend {:?} (bus_id={:?})",
            name,
            bus_id
        );

        self.backends.push((bus_id, backend));
    }

    /// Run early init for all backends.
    pub fn early_init_all(&mut self) {
        for (_id, backend) in &mut self.backends {
            if let Err(err) = backend.early_init() {
                log::warn!(
                    "bus backend {:?} early_init failed: {:?}",
                    backend.name(),
                    err
                );
            }
        }
    }

    /// Enumerate devices on every registered bus.
    pub fn enumerate_all(&mut self, context: &mut EnumerationContext) {
        for (bus_id, backend) in &mut self.backends {
            if let Err(err) = backend.enumerate(context, *bus_id) {
                log::warn!(
                    "bus backend {:?} enumerate failed: {:?}",
                    backend.name(),
                    err
                );
            }
        }
    }

    /// Re-scan all bus instances for hot-plugged or removed devices.
    pub fn rescan_all(&mut self, context: &mut EnumerationContext) {
        for (bus_id, backend) in &mut self.backends {
            if let Err(err) = backend.rescan(context, *bus_id) {
                log::warn!("bus backend {:?} rescan failed: {:?}", backend.name(), err);
            }
        }
    }

    /// Tear down every registered bus instance.
    ///
    /// Each backend's [`BusBackend::remove`] hook is invoked best-effort; the
    /// manager keeps going on failure so partial teardown does not strand the
    /// remaining buses.
    pub fn remove_all(&mut self) {
        for (bus_id, backend) in &mut self.backends {
            if let Err(err) = backend.remove(*bus_id) {
                log::warn!("bus backend {:?} remove failed: {:?}", backend.name(), err);
            }
        }
    }

    /// Quiesce every registered bus instance.
    ///
    /// Intended for the suspend / shutdown path: backends mask controller
    /// interrupts and stop background scanners so no further events arrive
    /// while higher layers drain.
    pub fn quiesce_all(&mut self) {
        for (bus_id, backend) in &mut self.backends {
            if let Err(err) = backend.quiesce(*bus_id) {
                log::warn!("bus backend {:?} quiesce failed: {:?}", backend.name(), err);
            }
        }
    }
}

impl Default for BusManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Build a [`BusManager`] pre-populated with the bus backends appropriate for
/// the current build configuration.
///
/// Registers built-in bus type matchers (PCI and Platform) before creating bus
/// instances, so that each backend finds its pre-registered matcher.
pub fn default_bus_manager() -> BusManager {
    use alloc::sync::Arc;

    // Register bus type matchers before any bus instances are created.
    kdevice::register_bus_type(Arc::new(kdevice::BusTypeObject::new(Arc::new(
        kdevice::PciBusTypeMatcher::new(),
    ))));
    kdevice::register_bus_type(Arc::new(kdevice::BusTypeObject::new(Arc::new(
        kdevice::PlatformBusTypeMatcher::new(),
    ))));

    let mut manager = BusManager::new();

    // Unified platform bus covering both firmware-described and compile-time
    // platform devices (console, pci-host, ramdisk, AHCI, etc.).
    manager.register(Box::new(super::platform_backend::PlatformBackend::new()));

    // PCI remains registered on all builds; the backend no-ops when no PCI host
    // is described by firmware or platform config.
    manager.register(Box::new(super::pci_backend::PciBackend::auto()));

    manager
}

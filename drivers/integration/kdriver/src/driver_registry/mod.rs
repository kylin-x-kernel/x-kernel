// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Built-in driver descriptor registration for the Linux-like device object model.

use alloc::sync::Arc;

use driver_base::{DeviceKind, DriverResult};
use kdevice::{DeviceDriver, DeviceId};

pub(crate) mod block;
#[cfg(feature = "char")]
pub(crate) mod char;
pub(crate) mod firmware_specs;
pub(crate) mod net;
pub(crate) mod virtio;

/// Re-export matcher result and priority types from `kdevice` for
/// convenience when implementing [`DeviceMatcher`](kdevice::DeviceMatcher).
pub use kdevice::{MatchResult, priority};

/// Convenience type alias for a shared driver implementation.
pub type BoxedDriver = Arc<dyn DeviceDriver>;

/// Factory function for one built-in driver descriptor.
pub type DriverFactory = fn() -> BoxedDriver;

/// Register every descriptor produced by `factories`.
pub fn register_factories(registrar: &mut DriverRegistrar, factories: &[DriverFactory]) {
    for factory in factories {
        registrar.register(factory());
    }
}

/// Register all enabled non-VirtIO platform and PCI driver descriptors.
pub fn register_platform_drivers(registrar: &mut DriverRegistrar) {
    block::register_all(registrar);
    net::register_all(registrar);
}

/// Lightweight summary of the current long-lived ownership graph visible at
/// the driver orchestration boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OwnershipSummary {
    pub bus_count: usize,
    pub driver_count: usize,
    pub device_count: usize,
}

/// Ownership summary for one bus visible at the driver orchestration boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BusOwnershipSummary {
    pub id: kdevice::BusId,
    pub name: &'static str,
    pub device_count: usize,
}

/// Ownership summary for one driver visible at the driver orchestration boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriverOwnershipSummary {
    pub id: kdevice::DriverId,
    pub name: &'static str,
    pub device_kind: DeviceKind,
    pub device_count: usize,
}

/// Thin registration facade for built-in driver descriptors.
pub struct DriverRegistrar;

impl DriverRegistrar {
    /// Create an empty driver registrar.
    pub fn new() -> Self {
        Self
    }

    /// Register a driver implementation with every supported bus type.
    pub fn register(&mut self, driver: BoxedDriver) {
        let object = kdevice::register_driver_object(driver);
        log::debug!(
            "registered driver: {} (driver_id={:?})",
            object.name(),
            object.id()
        );
    }

    /// Snapshot the current long-lived device topology from the driver core.
    pub fn current_ownership(&self) -> kdevice::DeviceTopology {
        kdevice::device_topology()
    }

    /// Summarize the current ownership graph visible to the driver registrar.
    pub fn current_ownership_summary(&self) -> OwnershipSummary {
        let ownership = self.current_ownership();
        OwnershipSummary {
            bus_count: ownership.bus_cores().count(),
            driver_count: ownership.driver_cores().count(),
            device_count: ownership.device_cores().count(),
        }
    }

    /// Run the driver-core remove path for one device.
    pub fn remove_device(&self, id: DeviceId) -> DriverResult<()> {
        kdevice::remove_device_managed(id)
    }
}

impl Default for DriverRegistrar {
    fn default() -> Self {
        Self::new()
    }
}

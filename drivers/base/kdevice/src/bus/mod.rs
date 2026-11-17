// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Long-lived bus instance object and instance metadata.

use alloc::{sync::Arc, vec::Vec};

use kspin::SpinNoPreempt;

use crate::{
    DeviceId, DeviceObject, DriverId, DriverObject,
    driver::{ProbeCounters, ProbeStats},
};

mod bus_type;

pub use bus_type::{
    BusType, BusTypeId, BusTypeObject, DriverMatchInfo, PciBusTypeMatcher, PlatformBusTypeMatcher,
};

/// Globally unique bus instance identifier assigned at runtime.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BusId(u64);

impl BusId {
    /// Wraps a raw numeric bus id.
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw numeric value.
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Persistent bus metadata.
#[derive(Clone, Debug)]
pub struct BusInfo {
    /// Unique bus instance ID.
    pub id: BusId,
    /// Bus type identifier used for driver matching.
    pub bus_type_id: BusTypeId,
    /// Human-readable bus instance name (e.g. `"pci"`, `"platform-static"`).
    pub name: &'static str,
}

/// Runtime bus instance registered by one discovery backend.
pub struct BusInstance {
    info: BusInfo,
    /// Upstream device that owns / produces this bus, if any.
    ///
    /// For a PCI bus this points at the host bridge `DeviceObject`. Buses
    /// that have no concrete controller device (e.g. the synthetic platform
    /// buses that aggregate firmware-described nodes) leave this `None`.
    controller: SpinNoPreempt<Option<Arc<DeviceObject>>>,
    devices: SpinNoPreempt<Vec<Arc<DeviceObject>>>,
    drivers: SpinNoPreempt<Vec<Arc<DriverObject>>>,
    probe: ProbeCounters,
}

impl BusInstance {
    /// Create a bus instance object.
    pub fn new(info: BusInfo) -> Self {
        Self {
            info,
            controller: SpinNoPreempt::new(None),
            devices: SpinNoPreempt::new(Vec::new()),
            drivers: SpinNoPreempt::new(Vec::new()),
            probe: ProbeCounters::new(),
        }
    }

    /// Persistent bus metadata.
    pub fn info(&self) -> &BusInfo {
        &self.info
    }

    /// Unique bus instance ID.
    pub const fn id(&self) -> BusId {
        self.info.id
    }

    /// Matching domain for devices on this bus.
    pub const fn bus_type(&self) -> BusTypeId {
        self.info.bus_type_id
    }

    /// Upstream controller device that owns this bus, if any.
    ///
    /// See [`BusInstance::set_controller`] for the registration side.
    pub fn controller(&self) -> Option<Arc<DeviceObject>> {
        self.controller.lock().clone()
    }

    /// Record (or clear) the controller device that owns this bus.
    ///
    /// Backends that adopt a real controller device (PCI host bridge, future
    /// I2C/USB controllers) call this once during enumeration. The matching
    /// forward-link lives on [`crate::DeviceObject::set_child_bus`].
    pub fn set_controller(&self, controller: Option<Arc<DeviceObject>>) {
        *self.controller.lock() = controller;
    }

    /// Attach a published device and its driver in one critical section.
    ///
    /// Driver-core-internal: invoked by the publish path (probe and adoption)
    /// after the registry has published the device and its binding metadata
    /// is visible via [`DeviceObject::driver_id`]. Taking `devices` then
    /// `drivers` as one critical section keeps the association consistent
    /// with a concurrent remove, which cannot remove the shared driver entry
    /// while this device is visible.
    pub(crate) fn attach_bound_device(&self, device: Arc<DeviceObject>, driver: Arc<DriverObject>) {
        // Unit tests pin the metadata-before-bus-attach invariant directly:
        // both publish paths must set `driver_id` before making the device
        // visible on the bus.
        #[cfg(unittest)]
        assert!(
            device.driver_id().is_some(),
            "bus attach requires binding metadata to be set first"
        );
        debug_assert!(
            device.driver_id().is_some(),
            "bus attach requires binding metadata to be set first"
        );
        let mut devices = self.devices.lock();
        if let Some(existing) = devices
            .iter_mut()
            .find(|existing| existing.id() == device.id())
        {
            *existing = device;
        } else {
            devices.push(device);
        }

        let mut drivers = self.drivers.lock();
        if !drivers.iter().any(|current| current.id() == driver.id()) {
            drivers.push(driver);
        }
    }

    /// Detach a device and orphan its driver association in one critical
    /// section.
    ///
    /// Driver-core-internal: invoked by the remove path after
    /// `begin_removing()` succeeded and before the device's binding metadata
    /// is cleared. The shared driver entry is removed only when no remaining
    /// device on this bus is still bound to it; checking and removing under
    /// the same `devices` guard prevents a concurrent publish from being
    /// missed.
    pub(crate) fn detach_device_and_orphan_driver(
        &self,
        id: DeviceId,
        driver: Option<DriverId>,
    ) -> Option<Arc<DeviceObject>> {
        let mut devices = self.devices.lock();
        let removed = devices
            .iter()
            .position(|device| device.id() == id)
            .map(|pos| devices.swap_remove(pos));

        if let Some(driver_id) = driver {
            let still_bound = devices
                .iter()
                .any(|device| device.driver_id() == Some(driver_id));
            if !still_bound {
                let mut drivers = self.drivers.lock();
                if let Some(pos) = drivers.iter().position(|current| current.id() == driver_id) {
                    drivers.swap_remove(pos);
                }
            }
        }
        removed
    }

    /// Devices currently attached to this bus instance.
    pub fn devices_snapshot(&self) -> Vec<Arc<DeviceObject>> {
        self.devices.lock().clone()
    }

    /// Drivers that have bound devices on this bus instance.
    pub fn drivers_snapshot(&self) -> Vec<Arc<DriverObject>> {
        self.drivers.lock().clone()
    }

    /// Record one dispatched probe attempt against a device on this bus.
    pub(crate) fn record_probe_attempt(&self) {
        self.probe.record_attempt();
    }

    /// Record one failed probe attempt against a device on this bus.
    pub(crate) fn record_probe_failure(&self) {
        self.probe.record_failure();
    }

    /// Snapshot this bus instance's probe accounting.
    pub fn probe_stats(&self) -> ProbeStats {
        self.probe.snapshot()
    }
}

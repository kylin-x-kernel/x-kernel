// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Read-only topology snapshot over the live device index.

use alloc::vec::Vec;

use crate::{
    BusHandle, BusId, BusInfo, DeviceCore, DeviceId, DeviceRecord, DriverCore, DriverId, DriverInfo,
};

/// Read-only view of one registered bus.
#[derive(Clone, Copy)]
pub struct BusView<'a> {
    /// Bus metadata: name and registered device kinds.
    pub info: &'a BusInfo,
}

/// Read-only view of one registered driver.
#[derive(Clone, Copy)]
pub struct DriverCoreView<'a> {
    /// Driver metadata: name, device kind, and id.
    pub info: &'a DriverInfo,
}

/// Read-only view of one device plus its resolved bus / driver relations.
#[derive(Clone, Copy)]
pub struct DeviceCoreView<'a> {
    /// Metadata snapshot of the device.
    pub record: &'a DeviceRecord,
    /// Bus instance the device lives on.
    pub bus: &'a BusInfo,
    /// Bound driver, once the device is bound.
    pub driver: Option<&'a DriverInfo>,
}

/// Unified read-only topology query surface for long-lived device relations.
#[derive(Clone)]
pub struct DeviceTopology {
    buses: Vec<BusInfo>,
    drivers: Vec<DriverInfo>,
    records: Vec<DeviceRecord>,
}

impl DeviceTopology {
    /// Snapshot the current live object graph from the driver-core index.
    pub fn snapshot() -> Self {
        Self {
            buses: crate::bus_infos_snapshot(),
            drivers: crate::driver_infos_snapshot(),
            records: crate::device_records_snapshot(),
        }
    }

    #[cfg(unittest)]
    pub(crate) fn from_parts(
        buses: Vec<BusInfo>,
        drivers: Vec<DriverInfo>,
        records: Vec<DeviceRecord>,
    ) -> Self {
        Self {
            buses,
            drivers,
            records,
        }
    }

    /// Iterate over all registered buses as long-lived core handles.
    pub fn bus_cores(&self) -> impl Iterator<Item = BusHandle> + '_ {
        self.buses.iter().map(|info| BusHandle::new(info.id))
    }

    /// Iterate over all registered drivers as long-lived core handles.
    pub fn driver_cores(&self) -> impl Iterator<Item = DriverCore> + '_ {
        self.drivers.iter().map(|info| DriverCore::new(info.id))
    }

    /// Iterate over all devices as long-lived core handles.
    pub fn device_cores(&self) -> impl Iterator<Item = DeviceCore> + '_ {
        self.records.iter().map(|record| DeviceCore::new(record.id))
    }

    /// Iterate over all registered buses.
    pub fn buses(&self) -> impl Iterator<Item = BusView<'_>> + '_ {
        self.buses.iter().map(|info| BusView { info })
    }

    /// Iterate over all registered drivers.
    pub fn drivers(&self) -> impl Iterator<Item = DriverCoreView<'_>> + '_ {
        self.drivers.iter().map(|info| DriverCoreView { info })
    }

    /// Iterate over all devices whose bus relation can be resolved.
    pub fn devices(&self) -> impl Iterator<Item = DeviceCoreView<'_>> + '_ {
        self.records
            .iter()
            .filter_map(move |record| resolve_device(self, record))
    }

    /// Resolve one device by id, including its bus / driver relations.
    pub fn device(&self, id: DeviceId) -> Option<DeviceCoreView<'_>> {
        self.records
            .iter()
            .find(|record| record.id == id)
            .and_then(|record| resolve_device(self, record))
    }

    /// Resolve one bus core handle to its current view.
    pub fn bus_core(&self, bus: BusHandle) -> Option<BusView<'_>> {
        self.buses
            .iter()
            .find(|info| info.id == bus.id())
            .map(|info| BusView { info })
    }

    /// Resolve one driver core handle to its current view.
    pub fn driver_core(&self, driver: DriverCore) -> Option<DriverCoreView<'_>> {
        self.drivers
            .iter()
            .find(|info| info.id == driver.id())
            .map(|info| DriverCoreView { info })
    }

    /// Resolve one device core handle to its current view.
    pub fn device_core(&self, device: DeviceCore) -> Option<DeviceCoreView<'_>> {
        self.device(device.id())
    }

    /// Iterate over all devices currently attached to one bus.
    pub fn devices_on_bus(&self, bus_id: BusId) -> impl Iterator<Item = DeviceCoreView<'_>> + '_ {
        self.records
            .iter()
            .filter(move |record| record.bus_id == bus_id)
            .filter_map(move |record| resolve_device(self, record))
    }

    /// Iterate over all devices currently attached to one bus core.
    pub fn devices_on_core_bus(
        &self,
        bus: BusHandle,
    ) -> impl Iterator<Item = DeviceCoreView<'_>> + '_ {
        self.devices_on_bus(bus.id())
    }

    /// Iterate over all devices currently associated with one driver.
    pub fn devices_for_driver(
        &self,
        driver_id: DriverId,
    ) -> impl Iterator<Item = DeviceCoreView<'_>> + '_ {
        self.records
            .iter()
            .filter(move |record| record.driver_id == Some(driver_id))
            .filter_map(move |record| resolve_device(self, record))
    }

    /// Iterate over all devices currently associated with one driver core.
    pub fn devices_for_core_driver(
        &self,
        driver: DriverCore,
    ) -> impl Iterator<Item = DeviceCoreView<'_>> + '_ {
        self.devices_for_driver(driver.id())
    }
}

/// Snapshot the current live driver-core topology.
pub fn device_topology() -> DeviceTopology {
    DeviceTopology::snapshot()
}

fn resolve_device<'a>(
    topology: &'a DeviceTopology,
    record: &'a DeviceRecord,
) -> Option<DeviceCoreView<'a>> {
    let bus = topology
        .buses
        .iter()
        .find(|info| info.id == record.bus_id)?;
    let driver = record
        .driver_id
        .and_then(|driver_id| topology.drivers.iter().find(|info| info.id == driver_id));
    Some(DeviceCoreView {
        record,
        bus,
        driver,
    })
}

unittest::path_mod_test!(tests, "../../ut-tests/topology.rs");

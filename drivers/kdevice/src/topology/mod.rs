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
    pub info: &'a BusInfo,
}

/// Read-only view of one registered driver.
#[derive(Clone, Copy)]
pub struct DriverCoreView<'a> {
    pub info: &'a DriverInfo,
}

/// Read-only view of one device plus its resolved bus / driver relations.
#[derive(Clone, Copy)]
pub struct DeviceCoreView<'a> {
    pub record: &'a DeviceRecord,
    pub bus: &'a BusInfo,
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

#[cfg(unittest)]
mod tests {
    use alloc::sync::Arc;

    use driver_base::DeviceKind;
    use unittest::{assert_eq, def_test};

    use super::*;
    use crate::{
        BusTypeId, DeviceIdentity, DeviceLocation, DeviceState, DiscoveryOrigin, PlatformIdentity,
        ResourceSet,
    };

    fn sample_record(id: u64, bus_id: BusId, driver_id: DriverId) -> DeviceRecord {
        DeviceRecord {
            id: DeviceId::new(id),
            bus_id,
            parent: None,
            child_bus: None,
            location: DeviceLocation::PlatformStatic { id: id as u16 },
            origin: DiscoveryOrigin::PlatformStatic,
            identity: DeviceIdentity::Platform(PlatformIdentity {
                alias: Some("topology-device"),
                firmware_id: None,
            }),
            transport: None,
            resources: crate::ResourceSet::new(),
            driver_name: Some("topology-driver"),
            driver_id: Some(driver_id),
            device_kind: Some(DeviceKind::Char),
            state: DeviceState::Active,
        }
    }

    #[def_test]
    fn test_topology_resolves_bus_and_driver_relations() {
        let bus_id = BusId::new(5);
        let driver_id = DriverId::new(7);
        let topology = DeviceTopology::from_parts(
            alloc::vec![BusInfo {
                id: bus_id,
                bus_type_id: BusTypeId::PLATFORM,
                name: "platform-static",
            }],
            alloc::vec![DriverInfo {
                id: driver_id,
                name: "topology-driver",
                device_kind: DeviceKind::Char,
            }],
            alloc::vec![sample_record(31, bus_id, driver_id)],
        );

        let device = topology.device(DeviceId::new(31)).unwrap();
        assert_eq!(device.record.id, DeviceId::new(31));
        assert_eq!(device.bus.id, bus_id);
        assert_eq!(device.driver.unwrap().id, driver_id);
        assert_eq!(
            topology.bus_core(BusHandle::new(bus_id)).unwrap().info.id,
            bus_id
        );
        assert_eq!(
            topology
                .driver_core(DriverCore::new(driver_id))
                .unwrap()
                .info
                .id,
            driver_id
        );
        assert_eq!(
            topology
                .device_core(DeviceCore::new(DeviceId::new(31)))
                .unwrap()
                .record
                .id,
            DeviceId::new(31)
        );
    }

    #[def_test]
    fn test_topology_filters_devices_by_bus_and_driver() {
        let bus_a = BusId::new(1);
        let bus_b = BusId::new(2);
        let driver_a = DriverId::new(11);
        let driver_b = DriverId::new(12);
        let topology = DeviceTopology::from_parts(
            alloc::vec![
                BusInfo {
                    id: bus_a,
                    bus_type_id: BusTypeId::PLATFORM,
                    name: "bus-a",
                },
                BusInfo {
                    id: bus_b,
                    bus_type_id: BusTypeId::PCI,
                    name: "bus-b",
                },
            ],
            alloc::vec![
                DriverInfo {
                    id: driver_a,
                    name: "driver-a",
                    device_kind: DeviceKind::Char,
                },
                DriverInfo {
                    id: driver_b,
                    name: "driver-b",
                    device_kind: DeviceKind::Block,
                },
            ],
            alloc::vec![
                sample_record(41, bus_a, driver_a),
                sample_record(42, bus_a, driver_b),
                sample_record(43, bus_b, driver_b),
            ],
        );

        assert_eq!(topology.bus_cores().count(), 2);
        assert_eq!(topology.driver_cores().count(), 2);
        assert_eq!(topology.device_cores().count(), 3);
        assert_eq!(topology.devices_on_bus(bus_a).count(), 2);
        assert_eq!(topology.devices_on_bus(bus_b).count(), 1);
        assert_eq!(topology.devices_for_driver(driver_a).count(), 1);
        assert_eq!(topology.devices_for_driver(driver_b).count(), 2);
        assert_eq!(
            topology.devices_on_core_bus(BusHandle::new(bus_a)).count(),
            2
        );
        assert_eq!(
            topology
                .devices_for_core_driver(DriverCore::new(driver_b))
                .count(),
            2
        );
    }

    #[def_test(serial)]
    fn test_topology_snapshots_live_index() {
        let (_bus_type, bus) = crate::reset_and_setup_platform_bus_for_tests("snapshot-bus");
        let device = {
            let mut index = crate::device_registry();
            let id = index.alloc_device_id();
            let device = Arc::new(crate::DeviceObject::new(
                id,
                bus.id(),
                DeviceLocation::PlatformStatic { id: 1 },
                DiscoveryOrigin::PlatformStatic,
                DeviceIdentity::Platform(PlatformIdentity {
                    alias: Some("snapshot-device"),
                    firmware_id: None,
                }),
                None,
                ResourceSet::new(),
            ));
            index.add_device(device.clone());
            device
        };

        let topology = DeviceTopology::snapshot();
        assert_eq!(topology.bus_cores().count(), 1);
        assert_eq!(topology.driver_cores().count(), 0);
        assert_eq!(topology.device_cores().count(), 1);
        assert_eq!(topology.device(device.id()).unwrap().bus.id, bus.id());
    }
}

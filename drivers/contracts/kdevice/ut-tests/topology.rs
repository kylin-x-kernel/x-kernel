// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

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

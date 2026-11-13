// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use driver_base::DeviceKind;
use unittest::{assert_eq, def_test};

use super::*;
use crate::{
    BusTypeId, DeviceIdentity, DeviceLocation, DeviceObject, DeviceState, DiscoveryOrigin,
    DriverError, PlatformIdentity, ResourceSet,
    topology::relations::attach_device_parent_locked,
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
        dma: None,
        intx: None,
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

fn topology_node(id: u64) -> Arc<DeviceObject> {
    let device = Arc::new(DeviceObject::new(
        DeviceId::new(id),
        BusId::new(0),
        DeviceLocation::PlatformStatic { id: id as u16 },
        DiscoveryOrigin::PlatformStatic,
        DeviceIdentity::Platform(PlatformIdentity {
            alias: Some("topology-node"),
            firmware_id: None,
        }),
        None,
        ResourceSet::new(),
    ));
    crate::device_registry().add_device(device.clone());
    device
}

fn registered(pairs: &[(u64, Option<u64>)]) {
    for &(id, _) in pairs {
        topology_node(id);
    }
    for &(id, parent) in pairs {
        let node = crate::find_device(DeviceId::new(id)).unwrap();
        node.set_parent(parent.map(DeviceId::new));
        if let Some(parent) = parent {
            crate::find_device(DeviceId::new(parent))
                .expect("every fixture node exists")
                .attach_child(node.id());
        }
    }
}

#[def_test(serial)]
fn test_attach_rejects_self_parent() {
    crate::reset_device_registry_for_tests();
    let node = topology_node(1);

    assert_eq!(
        attach_device_parent_locked(node.id(), node.id()),
        Err(DriverError::InvalidInput)
    );
    assert_eq!(node.parent(), None);
    assert!(node.children_snapshot().is_empty());
}

#[def_test(serial)]
fn test_attach_rejects_transitive_cycle() {
    crate::reset_device_registry_for_tests();
    let a = topology_node(1);
    let b = topology_node(2);
    let c = topology_node(3);

    // Parent chain A -> B -> C: A's parent is B, B's parent is C.
    attach_device_parent_locked(b.id(), a.id()).unwrap();
    attach_device_parent_locked(c.id(), b.id()).unwrap();

    // Making C a child of A would close the cycle A -> B -> C -> A.
    assert_eq!(
        attach_device_parent_locked(a.id(), c.id()),
        Err(DriverError::InvalidInput)
    );
    assert_eq!(a.parent(), Some(b.id()));
    assert_eq!(b.parent(), Some(c.id()));
    assert_eq!(c.parent(), None);
    assert!(!a.children_snapshot().contains(&c.id()));
}

#[def_test(serial)]
fn test_attach_replaces_old_parent_atomically() {
    crate::reset_device_registry_for_tests();
    let old = topology_node(1);
    let new = topology_node(2);
    let child = topology_node(3);

    attach_device_parent_locked(old.id(), child.id()).unwrap();
    assert_eq!(child.parent(), Some(old.id()));
    assert!(old.children_snapshot().contains(&child.id()));

    attach_device_parent_locked(new.id(), child.id()).unwrap();
    assert_eq!(child.parent(), Some(new.id()));
    assert!(!old.children_snapshot().contains(&child.id()));
    assert!(new.children_snapshot().contains(&child.id()));
}

#[def_test(serial)]
fn test_ancestor_walk_detects_existing_cycle() {
    crate::reset_device_registry_for_tests();
    // A <-> B: an existing cycle the lifecycle layer must detect rather than
    // assume the topology is acyclic.
    registered(&[(1, Some(2)), (2, Some(1))]);
    let a = crate::find_device(DeviceId::new(1)).unwrap();
    let b = crate::find_device(DeviceId::new(2)).unwrap();
    let child = topology_node(3);

    assert_eq!(
        attach_device_parent_locked(a.id(), child.id()),
        Err(DriverError::InvalidInput)
    );
    assert_eq!(child.parent(), None);
    // The fixture pre-populated A with B as a child; the rejected attach must
    // not add a second edge.
    assert!(!a.children_snapshot().contains(&child.id()));
    assert!(!b.children_snapshot().contains(&child.id()));
}

#[def_test(serial)]
fn test_ancestor_walk_depth_limit_returns_resource_busy() {
    crate::reset_device_registry_for_tests();
    // A 65-node chain has 64 traversable edges; starting the walk at the
    // deepest node exceeds the bounded depth.
    for id in 1..=65 {
        topology_node(id);
    }
    for id in 2..=65 {
        let parent = crate::find_device(DeviceId::new(id - 1)).unwrap();
        let child = crate::find_device(DeviceId::new(id)).unwrap();
        child.set_parent(Some(parent.id()));
        parent.attach_child(child.id());
    }
    let deepest = crate::find_device(DeviceId::new(65)).unwrap();
    let child = topology_node(100);

    assert_eq!(
        attach_device_parent_locked(deepest.id(), child.id()),
        Err(DriverError::ResourceBusy)
    );
    assert_eq!(child.parent(), None);
    assert_eq!(deepest.parent(), Some(DeviceId::new(64)));
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kdevice::{
    DeviceDescSpec, DeviceIdentity, DeviceLocation, DiscoveryOrigin, PlatformIdentity,
    ResourceSet,
};
use unittest::{assert_eq, def_test};

use super::*;

fn reset_model() -> alloc::sync::Arc<kdevice::BusInstance> {
    let (_bus_type, bus) = kdevice::reset_and_setup_platform_bus_for_tests("enumeration-test-bus");
    bus
}

#[def_test(serial)]
fn test_register_device_creates_descriptor_without_runtime_record() {
    let bus = reset_model();

    let mut context = EnumerationContext::new();
    let id = context
        .register_device(DeviceDescSpec {
            bus_id: bus.id(),
            parent: None,
            location: DeviceLocation::PlatformStatic { id: 1 },
            origin: DiscoveryOrigin::PlatformStatic,
            identity: DeviceIdentity::Platform(PlatformIdentity {
                alias: Some("test-device"),
                firmware_id: None,
            }),
            transport: None,
            resources: ResourceSet::new(),
            dma: None,
            intx: None,
            firmware_node: None,
        })
        .unwrap();

    let desc = kdevice::find_device_desc(id).unwrap();
    assert_eq!(desc.id(), id);
    assert_eq!(context.registered_count(), 1);
    assert_eq!(kdevice::device_records_snapshot().len(), 0);
}

#[def_test(serial)]
fn test_probe_pending_requeues_unmatched_descriptor() {
    let bus = reset_model();

    let mut context = EnumerationContext::new();
    context
        .register_device(DeviceDescSpec {
            bus_id: bus.id(),
            parent: None,
            location: DeviceLocation::PlatformStatic { id: 2 },
            origin: DiscoveryOrigin::PlatformStatic,
            identity: DeviceIdentity::Platform(PlatformIdentity {
                alias: Some("test-device"),
                firmware_id: None,
            }),
            transport: None,
            resources: ResourceSet::new(),
            dma: None,
            intx: None,
            firmware_node: None,
        })
        .unwrap();

    assert_eq!(context.probe_pending(), 0);
    assert_eq!(context.unclaimed_count(), 1);
    assert_eq!(
        context.unclaimed()[0].identity(),
        DeviceIdentity::Platform(PlatformIdentity {
            alias: Some("test-device"),
            firmware_id: None,
        })
    );
}

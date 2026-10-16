// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use driver_base::{DeviceKind, DriverResult};
use unittest::{assert_eq, def_test};

use super::*;
use crate::{DeviceDriver, DeviceState, PlatformIdentity};

struct MatchingDriver;

impl DeviceDriver for MatchingDriver {
    fn name(&self) -> &'static str {
        "matching-test"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Char
    }

    fn bus_types(&self) -> &'static [BusTypeId] {
        &[BusTypeId::PLATFORM]
    }

    fn matcher(&self) -> &dyn crate::DeviceMatcher {
        static M: crate::CompatibleAliasMatcher = crate::CompatibleAliasMatcher("matched");
        &M
    }

    fn probe_device(&self, _device: Arc<DeviceObject>) -> DriverResult<()> {
        Ok(())
    }
}

fn reset_model() -> Arc<BusInstance> {
    let (_bus_type, bus) = crate::reset_and_setup_platform_bus_for_tests("descriptor-test-bus");
    bus
}

#[def_test(serial)]
fn test_unmatched_descriptor_probe_does_not_create_runtime_object() {
    let bus = reset_model();
    let desc = device_desc_add(
        bus.id(),
        DeviceLocation::PlatformStatic { id: 1 },
        DiscoveryOrigin::PlatformStatic,
        DeviceIdentity::Platform(PlatformIdentity {
            alias: Some("unmatched"),
            firmware_id: None,
        }),
        None,
        ResourceSet::new(),
    )
    .unwrap();

    assert_eq!(crate::device_records_snapshot().len(), 0);
    assert_eq!(probe_device_desc(desc.id()).unwrap(), ProbeOutcome::Requeue);
    assert_eq!(crate::device_records_snapshot().len(), 0);
}

#[def_test(serial)]
fn test_driver_registration_scans_existing_descriptors() {
    let bus = reset_model();
    device_desc_add(
        bus.id(),
        DeviceLocation::PlatformStatic { id: 2 },
        DiscoveryOrigin::PlatformStatic,
        DeviceIdentity::Platform(PlatformIdentity {
            alias: Some("matched"),
            firmware_id: None,
        }),
        None,
        ResourceSet::new(),
    )
    .unwrap();

    assert_eq!(crate::device_records_snapshot().len(), 0);
    register_driver_object(Arc::new(MatchingDriver));

    let records = crate::device_records_snapshot();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].state, DeviceState::Active);
    assert_eq!(records[0].driver_name, Some("matching-test"));
}

#[def_test(serial)]
fn test_adopt_active_device_registers_bound_object() {
    let bus = reset_model();
    let driver = register_driver_object(Arc::new(MatchingDriver));

    let device = adopt_active_device(ActiveDeviceAdoption {
        bus_id: bus.id(),
        parent: None,
        location: DeviceLocation::PlatformStatic { id: 3 },
        origin: DiscoveryOrigin::PlatformStatic,
        identity: DeviceIdentity::Platform(PlatformIdentity {
            alias: Some("adopted"),
            firmware_id: None,
        }),
        transport: None,
        resources: ResourceSet::new(),
        driver,
    })
    .unwrap();

    let records = crate::device_records_snapshot();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].id, device.id());
    assert_eq!(records[0].state, DeviceState::Active);
    assert_eq!(records[0].driver_name, Some("matching-test"));

    let descriptors = crate::device_descs_snapshot();
    assert_eq!(descriptors.len(), 1);
    assert_eq!(
        descriptors[0].identity(),
        DeviceIdentity::Platform(PlatformIdentity {
            alias: Some("adopted"),
            firmware_id: None
        })
    );
    let desc_device = crate::device_registry()
        .find_device_for_desc(descriptors[0].id())
        .unwrap();
    assert_eq!(desc_device.id(), device.id());
}

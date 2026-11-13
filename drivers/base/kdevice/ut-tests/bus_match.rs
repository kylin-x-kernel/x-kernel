// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Bus-type matching regressions.

use alloc::sync::Arc;

use driver_base::{DeviceKind, DriverResult};
use unittest::def_test;

use super::*;
use crate::{
    BusTypeId, DeviceDesc, DeviceDescId, DeviceDescSpec, DeviceDriver, DeviceId, DeviceIdentity,
    DeviceLocation, DeviceMatcher, DeviceObject, DiscoveryOrigin, MatchResult, PciIdentity,
    PlatformIdentity, ResourceSet,
};

struct FixedMatcher(MatchResult);

impl DeviceMatcher for FixedMatcher {
    fn matches(&self, _desc: &DeviceDesc) -> MatchResult {
        self.0
    }
}

struct FixtureDriver {
    bus_types: &'static [BusTypeId],
    matcher: FixedMatcher,
}

impl DeviceDriver for FixtureDriver {
    fn name(&self) -> &'static str {
        "bus-match-fixture"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Char
    }

    fn bus_types(&self) -> &'static [BusTypeId] {
        self.bus_types
    }

    fn matcher(&self) -> &dyn DeviceMatcher {
        &self.matcher
    }

    fn probe_device(&self, _device: Arc<DeviceObject>) -> DriverResult<()> {
        Ok(())
    }
}

fn driver(bus_types: &'static [BusTypeId], matcher: MatchResult) -> FixtureDriver {
    FixtureDriver {
        bus_types,
        matcher: FixedMatcher(matcher),
    }
}

fn desc(location: DeviceLocation, identity: DeviceIdentity) -> DeviceDesc {
    DeviceDesc::from_spec(
        DeviceDescId::new(0),
        DeviceId::new(0),
        DeviceDescSpec {
            bus_id: crate::BusId::new(0),
            parent: None,
            location,
            origin: DiscoveryOrigin::PlatformStatic,
            identity,
            transport: None,
            resources: ResourceSet::new(),
            dma: None,
            intx: None,
            firmware_node: None,
        },
    )
}

fn pci_identity() -> DeviceIdentity {
    DeviceIdentity::Pci(PciIdentity {
        vendor_id: 0x1af4,
        device_id: 0x1042,
        class: 0,
        subclass: 0,
        subsystem_vendor_id: 0,
        subsystem_device_id: 0,
    })
}

fn platform_identity() -> DeviceIdentity {
    DeviceIdentity::Platform(PlatformIdentity {
        alias: Some("plat"),
        firmware_id: Some("vendor,plat"),
    })
}

fn pci_desc() -> DeviceDesc {
    desc(
        DeviceLocation::Pci {
            segment: 0,
            bus: 0,
            device: 1,
            function: 0,
        },
        pci_identity(),
    )
}

fn platform_desc(location: DeviceLocation) -> DeviceDesc {
    desc(location, platform_identity())
}

#[def_test]
fn pci_matcher_requires_pci_location_and_identity() {
    let matcher = PciBusTypeMatcher::new();
    let driver = driver(&[BusTypeId::PCI], MatchResult::Match);

    assert_eq!(
        matcher.match_desc(&driver, &pci_desc()),
        MatchResult::Match
    );

    // Location is platform, identity is PCI: rejected by the location check.
    let wrong_location = desc(
        DeviceLocation::PlatformStatic { id: 1 },
        pci_identity(),
    );
    assert_eq!(
        matcher.match_desc(&driver, &wrong_location),
        MatchResult::NoMatch
    );

    let wrong_identity = desc(
        DeviceLocation::Pci {
            segment: 0,
            bus: 0,
            device: 2,
            function: 0,
        },
        platform_identity(),
    );
    assert_eq!(
        matcher.match_desc(&driver, &wrong_identity),
        MatchResult::NoMatch
    );

    let bridge = desc(DeviceLocation::Bridge { domain: 0 }, pci_identity());
    assert_eq!(
        matcher.match_desc(&driver, &bridge),
        MatchResult::NoMatch
    );
}

#[def_test]
fn pci_matcher_requires_driver_bus_type() {
    let matcher = PciBusTypeMatcher::new();
    let platform_driver = driver(&[BusTypeId::PLATFORM], MatchResult::Match);
    assert_eq!(
        matcher.match_desc(&platform_driver, &pci_desc()),
        MatchResult::NoMatch
    );
}

#[def_test]
fn platform_matcher_accepts_firmware_static_and_mmio() {
    let matcher = PlatformBusTypeMatcher::new();
    let driver = driver(&[BusTypeId::PLATFORM], MatchResult::Match);

    for location in [
        DeviceLocation::FirmwareNode { id: 3 },
        DeviceLocation::PlatformStatic { id: 4 },
        DeviceLocation::Mmio {
            base: 0x1000,
            size: 0x100,
        },
    ] {
        assert_eq!(
            matcher.match_desc(&driver, &platform_desc(location)),
            MatchResult::Match,
            "location {location:?} should match"
        );
    }
}

#[def_test]
fn platform_matcher_rejects_bridge_and_foreign_bus_type() {
    let matcher = PlatformBusTypeMatcher::new();
    let platform_driver = driver(&[BusTypeId::PLATFORM], MatchResult::Match);

    let bridge = desc(DeviceLocation::Bridge { domain: 1 }, platform_identity());
    assert_eq!(
        matcher.match_desc(&platform_driver, &bridge),
        MatchResult::NoMatch
    );

    let pci_driver = driver(&[BusTypeId::PCI], MatchResult::Match);
    let static_desc = platform_desc(DeviceLocation::PlatformStatic { id: 5 });
    assert_eq!(
        matcher.match_desc(&pci_driver, &static_desc),
        MatchResult::NoMatch
    );
}

#[def_test]
fn driver_matcher_no_match_is_propagated() {
    let matcher = PlatformBusTypeMatcher::new();
    let driver = driver(&[BusTypeId::PLATFORM], MatchResult::NoMatch);
    let static_desc = platform_desc(DeviceLocation::PlatformStatic { id: 6 });
    assert_eq!(
        matcher.match_desc(&driver, &static_desc),
        MatchResult::NoMatch
    );
}

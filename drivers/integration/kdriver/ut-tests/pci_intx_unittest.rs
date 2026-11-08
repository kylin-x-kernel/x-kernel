// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use pci::Cam;
use unittest::{assert_eq, def_test};

use super::*;

#[repr(align(4096))]
struct BusWindow([u8; 0x800]);

fn function() -> DeviceFunction {
    DeviceFunction {
        bus: 0,
        device: 0,
        function: 0,
    }
}

fn config(window: &mut BusWindow) -> PciConfigAccess {
    // SAFETY: the stack window stays alive for the test, is page aligned, and
    // the bound matches its size.
    unsafe { PciConfigAccess::new_bounded(window.0.as_mut_ptr(), Cam::Ecam, 0x800) }
}

#[def_test]
fn absent_pin_reports_not_connected_without_querying_firmware() {
    let mut window = BusWindow([0; 0x800]);
    let config = config(&mut window);
    for pin in [0, 0xff] {
        assert_eq!(config.try_write(function(), 0x3D, 1, pin), Ok(()));
        assert_eq!(
            resolve(&config, function(), DiscoveryOrigin::DeviceTree),
            Err(IntxRouteStatus::NotConnected)
        );
    }
}

#[def_test]
fn pin_without_acpi_origin_is_a_route_failure() {
    let mut window = BusWindow([0; 0x800]);
    let config = config(&mut window);
    assert_eq!(config.try_write(function(), 0x3D, 1, 1), Ok(()));
    assert_eq!(
        resolve(&config, function(), DiscoveryOrigin::DeviceTree),
        Err(IntxRouteStatus::Failed)
    );
}

#[def_test]
fn unreadable_pin_is_a_route_failure() {
    let mut window = BusWindow([0; 0x800]);
    let config = config(&mut window);
    let invalid_bdf = DeviceFunction {
        device: 32,
        ..function()
    };
    assert_eq!(
        resolve(&config, invalid_bdf, DiscoveryOrigin::Acpi),
        Err(IntxRouteStatus::Failed)
    );
}

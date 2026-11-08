// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use driver_base::DriverError;
use driver_os_api::{IrqResource, Virq};
use khal::firmware::devices::{FwError, InterruptControllerKind, InterruptTrigger};
use pci::LegacyInterruptRoute;
use unittest::{assert_eq, def_test};

#[def_test]
fn unreadable_firmware_declarations_never_look_like_absent_irqs() {
    for (firmware, expected) in [
        (FwError::InvalidNode, DriverError::InvalidInput),
        (FwError::Missing, DriverError::InvalidInput),
        (FwError::InvalidValue, DriverError::InvalidInput),
        (FwError::BufferTooSmall, DriverError::InvalidInput),
        (FwError::Unsupported, DriverError::Unsupported),
    ] {
        assert_eq!(super::firmware_error_to_driver(firmware), expected);
    }
}

#[def_test]
fn gsi_route_goes_through_map_gsi_not_the_hwirq_path() {
    let seen = core::cell::Cell::new(None);
    let mapped = super::map_pci_legacy_irq(
        LegacyInterruptRoute::Gsi {
            gsi: 28,
            trigger: InterruptTrigger::LevelLow,
        },
        |_| panic!("a GSI must not become an IrqRouteDesc hwirq"),
        |gsi, trigger| {
            seen.set(Some((gsi, trigger)));
            Ok(IrqResource::plain(Virq::new(9), trigger))
        },
    )
    .expect("GSI mapping accepted");
    assert_eq!(seen.get(), Some((28, kdevice::IrqTrigger::LevelLow)));
    assert_eq!(mapped.virq().raw(), 9);
}

#[def_test]
fn hwirq_route_keeps_the_irq_route_desc_path() {
    let seen = core::cell::Cell::new(None);
    let mapped = super::map_pci_legacy_irq(
        LegacyInterruptRoute::Hwirq {
            hwirq: 35,
            trigger: InterruptTrigger::LevelHigh,
            controller: InterruptControllerKind::Gic,
        },
        |route| {
            seen.set(Some(route));
            Ok(IrqResource::plain(Virq::new(7), route.trigger))
        },
        |_, _| panic!("a DT hwirq route must not be resolved as a GSI"),
    )
    .expect("hwirq mapping accepted");
    let route = seen.get().expect("map_irq was called");
    assert_eq!(route.hwirq, 35);
    assert_eq!(route.trigger, kdevice::IrqTrigger::LevelHigh);
    assert_eq!(route.controller, driver_os_api::IrqController::Gic);
    assert_eq!(route.domain, None);
    assert_eq!(mapped.virq().raw(), 7);
}

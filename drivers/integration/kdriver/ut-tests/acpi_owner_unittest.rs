// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Host tests for the ACPI INTx owner's route conversion.

use unittest::{assert_eq, def_test};

use super::*;

fn bdf(device: u8) -> DeviceFunction {
    DeviceFunction {
        bus: 0,
        device,
        function: 0,
    }
}

#[def_test]
fn route_lookup_preserves_gsi_and_trigger() {
    let route = route_from_lookup(Ok(Some(PrtRoute {
        gsi: 28,
        trigger: PrtTrigger::LevelLow,
    })))
    .expect("route accepted");
    assert_eq!(
        route,
        Some(LegacyInterruptRoute::Gsi {
            gsi: 28,
            trigger: pci::InterruptTrigger::LevelLow,
        })
    );

    assert_eq!(route_from_lookup(Ok(None)), Ok(None));
    assert_eq!(
        route_from_lookup(Err(PrtLookupError::UnusableCrs)),
        Err(RouteFailure::Lookup(PrtLookupError::UnusableCrs))
    );
}

#[def_test]
fn every_prt_trigger_maps_to_the_firmware_trigger() {
    for (prt, firmware) in [
        (PrtTrigger::EdgeRising, pci::InterruptTrigger::EdgeRising),
        (PrtTrigger::EdgeFalling, pci::InterruptTrigger::EdgeFalling),
        (PrtTrigger::LevelHigh, pci::InterruptTrigger::LevelHigh),
        (PrtTrigger::LevelLow, pci::InterruptTrigger::LevelLow),
    ] {
        assert_eq!(prt_trigger_to_firmware(prt), firmware);
    }
}

#[def_test]
fn route_before_initialization_reports_not_initialized() {
    // Boot enumeration initializes the process-wide owner before tests run, so
    // the uninitialized state is injected rather than read from the static.
    assert_eq!(
        route_from_owner(None, bdf(1), 1),
        Err(RouteFailure::NotInitialized)
    );
    assert_eq!(
        route_from_owner(
            Some(&OwnerState::Failed(InitFailure::Prt)),
            bdf(1),
            1
        ),
        Err(RouteFailure::Init(InitFailure::Prt))
    );
}

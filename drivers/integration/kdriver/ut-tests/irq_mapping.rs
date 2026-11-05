// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use driver_base::DriverError;
use driver_os_api::{IrqController, IrqResource, IrqTrigger, ResError, Virq};
use khal::firmware::devices::{InterruptControllerKind, InterruptInfo, InterruptTrigger};
use unittest::{assert, assert_eq, def_test};

#[def_test]
fn firmware_irq_uses_provider_identity_and_preserves_route_metadata() {
    for (firmware, controller) in [
        (InterruptControllerKind::Gic, IrqController::Gic),
        (InterruptControllerKind::Plic, IrqController::Plic),
        (InterruptControllerKind::Unknown, IrqController::Unknown),
    ] {
        for (trigger, expected) in [
            (InterruptTrigger::EdgeRising, IrqTrigger::EdgeRising),
            (InterruptTrigger::EdgeFalling, IrqTrigger::EdgeFalling),
            (InterruptTrigger::LevelHigh, IrqTrigger::LevelHigh),
            (InterruptTrigger::LevelLow, IrqTrigger::LevelLow),
            (InterruptTrigger::Unknown(17), IrqTrigger::Unknown(17)),
        ] {
            let mut observed = None;
            let mapped = IrqResource::DomainMapped {
                virq: Virq::new(4097),
                trigger: expected,
                controller,
                domain: None,
                hwirq: 48,
            };
            let result = super::map_firmware_irq(
                InterruptInfo {
                    irq: 48,
                    trigger,
                    controller: firmware,
                },
                |route| {
                    observed = Some(route);
                    Ok(mapped)
                },
            );
            assert_eq!(result, Ok(mapped));
            let route = observed.unwrap();
            assert_eq!(route.hwirq, 48);
            assert_eq!(route.trigger, expected);
            assert_eq!(route.controller, controller);
            assert!(route.domain.is_none());
        }
    }
}

#[def_test]
fn mapping_failure_never_falls_back_to_the_firmware_number() {
    for (failure, expected) in [
        (ResError::InvalidResource, DriverError::InvalidInput),
        (ResError::MappingFailed, DriverError::Io),
        (ResError::NoMemory, DriverError::NoMemory),
        (ResError::Busy, DriverError::ResourceBusy),
        (ResError::Unsupported, DriverError::Unsupported),
    ] {
        let result = super::map_firmware_irq(
            InterruptInfo {
                irq: 48,
                trigger: InterruptTrigger::LevelHigh,
                controller: InterruptControllerKind::Gic,
            },
            |_| Err(failure),
        );
        assert_eq!(result, Err(expected));
    }
}

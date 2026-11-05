// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Bus discovery backends (execution layer).
//!
//! This module contains concrete discovery implementations and probing glue.
//! Persistent metadata types (`BusId`, `BusTypeId`, `BusInfo`) stay in
//! `kdevice` as part of the shared model layer.

use driver_base::DriverResult;
use driver_os_api::{IrqController, IrqResource, IrqRouteDesc, ResResult};
use kdevice::IrqTrigger;
use khal::firmware::devices::{InterruptControllerKind, InterruptInfo};

pub(crate) mod backend;
mod local_id;
pub(crate) mod manager;
pub(crate) mod pci_backend;
pub(crate) mod pci_support;
pub(crate) mod platform_backend;

// Firmware numbers are controller-local, not registration-ready logical IRQs.
// Keep mapping in bus integration; neither the firmware parser nor a concrete
// driver owns host IRQ descriptor preparation.
fn map_firmware_irq(
    irq: InterruptInfo,
    map_irq: impl FnOnce(IrqRouteDesc) -> ResResult<IrqResource>,
) -> DriverResult<IrqResource> {
    let controller = match irq.controller {
        InterruptControllerKind::Gic => IrqController::Gic,
        InterruptControllerKind::Plic => IrqController::Plic,
        InterruptControllerKind::Unknown => IrqController::Unknown,
    };
    map_irq(IrqRouteDesc {
        hwirq: irq.irq,
        trigger: irq_trigger_from_firmware(irq.trigger),
        controller,
        domain: None,
    })
    .map_err(crate::resource::map_res_err)
}

fn irq_trigger_from_firmware(trigger: khal::firmware::devices::InterruptTrigger) -> IrqTrigger {
    match trigger {
        khal::firmware::devices::InterruptTrigger::EdgeRising => IrqTrigger::EdgeRising,
        khal::firmware::devices::InterruptTrigger::EdgeFalling => IrqTrigger::EdgeFalling,
        khal::firmware::devices::InterruptTrigger::LevelHigh => IrqTrigger::LevelHigh,
        khal::firmware::devices::InterruptTrigger::LevelLow => IrqTrigger::LevelLow,
        khal::firmware::devices::InterruptTrigger::Unknown(flags) => IrqTrigger::Unknown(flags),
    }
}

unittest::path_mod_test!(tests_irq_mapping, "../../ut-tests/irq_mapping.rs");

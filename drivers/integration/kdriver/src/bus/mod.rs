// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Bus discovery backends (execution layer).
//!
//! This module contains concrete discovery implementations and probing glue.
//! Persistent metadata types (`BusId`, `BusTypeId`, `BusInfo`) stay in
//! `kdevice` as part of the shared model layer.

use driver_base::{DriverError, DriverResult};
use driver_os_api::{IrqController, IrqResource, IrqRouteDesc, ResResult};
use kdevice::IrqTrigger;
use khal::firmware::devices::{FwError, InterruptControllerKind, InterruptInfo};

pub(crate) mod backend;
mod local_id;
pub(crate) mod manager;
pub(crate) mod pci_backend;
pub(crate) mod pci_support;
pub(crate) mod platform_backend;

/// Map a PCI legacy INTx route.
///
/// PCI firmware routing has not migrated to the host provider's firmware
/// declarations yet: it still receives a decoded
/// [`InterruptInfo`] from the `of` compatibility layer and guesses the
/// controller family here. New firmware paths must use `IrqOp::map_fw_irq`,
/// which preserves the controller node and full binding cells.
fn map_pci_legacy_irq(
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

/// A firmware declaration that could not even be read is a device-description
/// error, not "no interrupt": callers must not fall back to polling or to a
/// raw firmware number.
fn firmware_error_to_driver(error: FwError) -> DriverError {
    match error {
        FwError::Unsupported => DriverError::Unsupported,
        FwError::InvalidNode
        | FwError::Missing
        | FwError::InvalidValue
        | FwError::BufferTooSmall => DriverError::InvalidInput,
    }
}

unittest::path_mod_test!(tests_irq_mapping, "../../ut-tests/irq_mapping.rs");

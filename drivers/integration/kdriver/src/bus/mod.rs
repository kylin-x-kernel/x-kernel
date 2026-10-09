// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Bus discovery backends (execution layer).
//!
//! This module contains concrete discovery implementations and probing glue.
//! Persistent metadata types (`BusId`, `BusTypeId`, `BusInfo`) stay in
//! `kdevice` as part of the shared model layer.

use kdevice::IrqTrigger;

pub(crate) mod backend;
mod local_id;
pub(crate) mod manager;
pub(crate) mod pci_backend;
pub(crate) mod pci_support;
pub(crate) mod platform_backend;

fn irq_trigger_from_firmware(trigger: khal::firmware::devices::InterruptTrigger) -> IrqTrigger {
    match trigger {
        khal::firmware::devices::InterruptTrigger::EdgeRising => IrqTrigger::EdgeRising,
        khal::firmware::devices::InterruptTrigger::EdgeFalling => IrqTrigger::EdgeFalling,
        khal::firmware::devices::InterruptTrigger::LevelHigh => IrqTrigger::LevelHigh,
        khal::firmware::devices::InterruptTrigger::LevelLow => IrqTrigger::LevelLow,
        khal::firmware::devices::InterruptTrigger::Unknown(flags) => IrqTrigger::Unknown(flags),
    }
}

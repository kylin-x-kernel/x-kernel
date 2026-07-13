// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unified resource descriptors for device discovery.
//!
//! The descriptor types themselves are OS-agnostic and live in [`device_res`].
//! This module re-exports them and provides host-specific conversions from the
//! kernel's firmware/IRQ trigger representations, which cannot live in the
//! OS-neutral crate without coupling it to `khal`.

pub use device_res::{
    DmaSpec, IoPortRange, IrqResource, IrqTrigger, MmioRegion, ResourceDesc, ResourceSet,
};

/// Convert a firmware-described interrupt trigger into an [`IrqTrigger`].
pub fn irq_trigger_from_firmware(trigger: khal::firmware::devices::InterruptTrigger) -> IrqTrigger {
    match trigger {
        khal::firmware::devices::InterruptTrigger::EdgeRising => IrqTrigger::EdgeRising,
        khal::firmware::devices::InterruptTrigger::EdgeFalling => IrqTrigger::EdgeFalling,
        khal::firmware::devices::InterruptTrigger::LevelHigh => IrqTrigger::LevelHigh,
        khal::firmware::devices::InterruptTrigger::LevelLow => IrqTrigger::LevelLow,
        khal::firmware::devices::InterruptTrigger::Unknown(f) => IrqTrigger::Unknown(f),
    }
}

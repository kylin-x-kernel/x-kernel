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
    DmaSpec, IoPortRange, IrqResource, IrqTriggerMode, MmioRegion, ResourceDesc, ResourceSet,
};

/// Convert a firmware-described interrupt trigger into an [`IrqTriggerMode`].
pub fn irq_trigger_from_firmware(
    trigger: khal::firmware::devices::InterruptTrigger,
) -> IrqTriggerMode {
    match trigger {
        khal::firmware::devices::InterruptTrigger::EdgeRising => IrqTriggerMode::EdgeRising,
        khal::firmware::devices::InterruptTrigger::EdgeFalling => IrqTriggerMode::EdgeFalling,
        khal::firmware::devices::InterruptTrigger::LevelHigh => IrqTriggerMode::LevelHigh,
        khal::firmware::devices::InterruptTrigger::LevelLow => IrqTriggerMode::LevelLow,
        khal::firmware::devices::InterruptTrigger::Unknown(_) => IrqTriggerMode::Unspecified,
    }
}

/// Convert a `khal` IRQ trigger into an [`IrqTriggerMode`].
pub fn irq_trigger_from_khal(trigger: khal::irq::IrqTrigger) -> IrqTriggerMode {
    match trigger {
        khal::irq::IrqTrigger::EdgeRising => IrqTriggerMode::EdgeRising,
        khal::irq::IrqTrigger::EdgeFalling => IrqTriggerMode::EdgeFalling,
        khal::irq::IrqTrigger::LevelHigh => IrqTriggerMode::LevelHigh,
        khal::irq::IrqTrigger::LevelLow => IrqTriggerMode::LevelLow,
        khal::irq::IrqTrigger::Unknown(_) => IrqTriggerMode::Unspecified,
    }
}

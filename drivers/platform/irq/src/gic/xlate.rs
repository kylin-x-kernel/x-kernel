// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! GIC interrupt-cell translation owned by the GIC driver.

use arm_gic_driver::{PPI_RANGE, SPI_RANGE};
use kirq::{IrqDomainXlate, IrqHwSpec, IrqTrigger, IrqXlateError};

/// Firmware interrupt type cell: shared peripheral interrupt.
const SPI_TYPE: u32 = 0;
/// Firmware interrupt type cell: private peripheral interrupt.
const PPI_TYPE: u32 = 1;
/// Firmware interrupt type cell: extended SPI, unsupported by this driver.
const ESPI_TYPE: u32 = 2;

/// GIC binding translator registered with the GIC IRQ domain.
pub(super) struct GicXlate;

pub(super) static GIC_XLATE: GicXlate = GicXlate;

impl IrqDomainXlate for GicXlate {
    fn translate(&self, params: &[u32]) -> Result<IrqHwSpec, IrqXlateError> {
        let &[interrupt_type, number, flags] = params else {
            return Err(IrqXlateError::Malformed);
        };
        let hwirq = match interrupt_type {
            SPI_TYPE => (SPI_RANGE.start as usize)
                .checked_add(number as usize)
                .filter(|hwirq| *hwirq < SPI_RANGE.end as usize)
                .ok_or(IrqXlateError::InvalidHwirq)?,
            PPI_TYPE => (PPI_RANGE.start as usize)
                .checked_add(number as usize)
                .filter(|hwirq| *hwirq < PPI_RANGE.end as usize)
                .ok_or(IrqXlateError::InvalidHwirq)?,
            ESPI_TYPE => return Err(IrqXlateError::Unsupported),
            _ => return Err(IrqXlateError::Malformed),
        };
        Ok(IrqHwSpec {
            hwirq,
            trigger: translate_flags(flags)?,
        })
    }
}

fn translate_flags(flags: u32) -> Result<IrqTrigger, IrqXlateError> {
    match flags & 0xf {
        1 => Ok(IrqTrigger::EdgeRising),
        2 => Ok(IrqTrigger::EdgeFalling),
        4 => Ok(IrqTrigger::LevelHigh),
        8 => Ok(IrqTrigger::LevelLow),
        // The binding declares no trigger; the controller keeps that as unknown
        // instead of inventing a default the firmware did not state.
        0 => Ok(IrqTrigger::Unknown(0)),
        // Multiple or reserved sense bits are not a valid GIC trigger.
        _ => Err(IrqXlateError::Malformed),
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/gic_xlate.rs");

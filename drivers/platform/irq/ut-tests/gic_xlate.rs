// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kirq::{IrqDomainXlate, IrqTrigger, IrqXlateError};
use unittest::{assert_eq, def_test};

use super::GIC_XLATE;

#[def_test]
fn gic_cells_decode_spi_and_ppi_numbers_with_binding_offsets() {
    assert_eq!(
        GIC_XLATE.translate(&[0, 0, 4]),
        Ok(kirq::IrqHwSpec {
            hwirq: 32,
            trigger: IrqTrigger::LevelHigh,
        })
    );
    assert_eq!(
        GIC_XLATE.translate(&[0, 987, 1]),
        Ok(kirq::IrqHwSpec {
            hwirq: 1019,
            trigger: IrqTrigger::EdgeRising,
        })
    );
    for (number, hwirq) in [(0, 16), (15, 31)] {
        assert_eq!(
            GIC_XLATE.translate(&[1, number, 2]),
            Ok(kirq::IrqHwSpec {
                hwirq,
                trigger: IrqTrigger::EdgeFalling,
            })
        );
    }
}

#[def_test]
fn gic_cells_preserve_every_defined_trigger_and_unknown_default() {
    for (flags, trigger) in [
        (1, IrqTrigger::EdgeRising),
        (2, IrqTrigger::EdgeFalling),
        (4, IrqTrigger::LevelHigh),
        (8, IrqTrigger::LevelLow),
        (0, IrqTrigger::Unknown(0)),
    ] {
        assert_eq!(
            GIC_XLATE.translate(&[0, 3, flags]),
            Ok(kirq::IrqHwSpec { hwirq: 35, trigger })
        );
    }
}

#[def_test]
fn gic_cells_reject_malformed_unsupported_and_out_of_range_specifiers() {
    for cells in [&[][..], &[0][..], &[0, 3][..], &[0, 3, 4, 0][..], &[3, 3, 4][..]] {
        assert_eq!(GIC_XLATE.translate(cells), Err(IrqXlateError::Malformed));
    }
    assert_eq!(
        GIC_XLATE.translate(&[2, 0, 4]),
        Err(IrqXlateError::Unsupported)
    );
    assert_eq!(
        GIC_XLATE.translate(&[0, 988, 4]),
        Err(IrqXlateError::InvalidHwirq)
    );
    assert_eq!(
        GIC_XLATE.translate(&[0, u32::MAX, 4]),
        Err(IrqXlateError::InvalidHwirq)
    );
    assert_eq!(
        GIC_XLATE.translate(&[1, 16, 4]),
        Err(IrqXlateError::InvalidHwirq)
    );
    for flags in [3, 5, 6, 7, 9, 15] {
        assert_eq!(
            GIC_XLATE.translate(&[0, 3, flags]),
            Err(IrqXlateError::Malformed)
        );
    }
}

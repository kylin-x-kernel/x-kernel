// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use fwnode::{FW_IRQ_SPEC_MAX_PARAMS, FwError, FwParams, FwProperty, FwPropertyValue};

#[test]
fn firmware_cells_are_big_endian() {
    let property = FwProperty {
        name: "#interrupt-cells",
        value: FwPropertyValue::Bytes(&[0, 0, 0, 3]),
    };
    assert_eq!(property.as_u32(), Ok(3));
    assert_eq!(property.as_u64(), Err(FwError::InvalidValue));
}

#[test]
fn strings_stop_at_the_first_nul() {
    let property = FwProperty {
        name: "status",
        value: FwPropertyValue::Bytes(b"okay\0"),
    };
    assert_eq!(property.as_str(), Ok("okay"));
    assert_eq!(property.as_bool(), Err(FwError::InvalidValue));
}

#[test]
fn flag_and_integer_booleans() {
    assert_eq!(
        FwProperty {
            name: "interrupt-controller",
            value: FwPropertyValue::Empty,
        }
        .as_bool(),
        Ok(true)
    );
    assert_eq!(
        FwProperty {
            name: "enabled",
            value: FwPropertyValue::U32(0),
        }
        .as_bool(),
        Ok(false)
    );
}

#[test]
fn irq_params_are_capacity_checked() {
    let values = [7; FW_IRQ_SPEC_MAX_PARAMS];
    let params = FwParams::new(&values).unwrap();
    assert_eq!(params.len(), FW_IRQ_SPEC_MAX_PARAMS);
    assert_eq!(params.get(0), Some(7));
    assert_eq!(params.get(FW_IRQ_SPEC_MAX_PARAMS), None);

    let too_many = [0; FW_IRQ_SPEC_MAX_PARAMS + 1];
    assert_eq!(FwParams::new(&too_many), Err(FwError::BufferTooSmall));
}

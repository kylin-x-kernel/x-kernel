// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use driver_base::DriverError;
use khal::firmware::devices::FwError;
use unittest::{assert_eq, def_test};

#[def_test]
fn unreadable_firmware_declarations_never_look_like_absent_irqs() {
    for (firmware, expected) in [
        (FwError::InvalidNode, DriverError::InvalidInput),
        (FwError::Missing, DriverError::InvalidInput),
        (FwError::InvalidValue, DriverError::InvalidInput),
        (FwError::BufferTooSmall, DriverError::InvalidInput),
        (FwError::Unsupported, DriverError::Unsupported),
    ] {
        assert_eq!(super::firmware_error_to_driver(firmware), expected);
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::dupfd_minimum_from_arg;

#[def_test]
fn test_dupfd_minimum_uses_signed_low_32_bits() {
    assert_eq!(dupfd_minimum_from_arg(64), 64);
    assert_eq!(dupfd_minimum_from_arg(u32::MAX as usize), -1);
    assert_eq!(dupfd_minimum_from_arg(0x8000_0000), i32::MIN);
}

#[def_test]
fn test_dupfd_minimum_ignores_high_argument_bits() {
    if usize::BITS > 32 {
        let high = 1_u64 << 32;
        assert_eq!(dupfd_minimum_from_arg((high | 64) as usize), 64);
        assert_eq!(
            dupfd_minimum_from_arg((high | u32::MAX as u64) as usize),
            -1
        );
    }
}

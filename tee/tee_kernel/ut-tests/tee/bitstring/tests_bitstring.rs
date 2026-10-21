// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::assert_eq;

use super::*;

#[unittest::def_test]
fn test_bit_ffc() {
    let mut val: isize;

    let bits = [0x00];
    val = -2;
    bit_ffc(&bits, 8, &mut val);
    assert_eq!(val, 0);

    let bits = [0x01];
    val = -2;
    bit_ffc(&bits, 8, &mut val);
    assert_eq!(val, 1);

    let bits = [0xff];
    val = -2;
    bit_ffc(&bits, 8, &mut val);
    assert_eq!(val, -1);

    let bits = [0xff, 0b11110111];
    val = -2;
    bit_ffc(&bits, 16, &mut val);
    assert_eq!(val, 11);

    let bits = [0x7f];
    val = -2;
    bit_ffc(&bits, 7, &mut val);
    assert_eq!(val, -1);
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::assert_eq;

use super::*;

#[unittest::def_test]
fn test_slice_fmt() {
    let data = [0x12, 0x34, 0x56, 0x78];
    let result = slice_fmt(&data);
    assert_eq!(result, "len: 0x4, data: 12345678");
}

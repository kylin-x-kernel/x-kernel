// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

#[def_test]
fn test_align_helpers() {
    assert_eq!(floor_align(0x1234, 0x1000), 0x1000);
    assert_eq!(ceil_align(0x1234, 0x1000), 0x2000);
    assert_eq!(align_rem(0x1234, 0x1000), 0x234);
}

#[def_test]
fn test_align_4k_helpers() {
    assert_eq!(floor_4k(0x1fff), 0x1000);
    assert_eq!(ceil_4k(0x1001), 0x2000);
    assert_eq!(rem_4k(0x1001), 0x1);
}

#[def_test]
fn test_aligned_checks() {
    assert!(aligned_to(0x2000, 0x1000));
    assert!(!aligned_to(0x2001, 0x1000));
    assert!(aligned_4k(0x3000));
}

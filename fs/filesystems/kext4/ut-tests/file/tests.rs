// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::normalize_write_allocation_len;

#[def_test]
fn normalized_allocation_keeps_single_block_writes_exact() {
    assert_eq!(normalize_write_allocation_len(1, 64, true), 1);
    assert_eq!(normalize_write_allocation_len(1, 64, false), 1);
}

#[def_test]
fn normalized_allocation_expands_streaming_requests_within_hole() {
    assert_eq!(normalize_write_allocation_len(4, 128, true), 32);
    assert_eq!(normalize_write_allocation_len(4, 10, true), 10);
}

#[def_test]
fn normalized_allocation_caps_random_requests() {
    assert_eq!(normalize_write_allocation_len(2, 128, false), 2);
    assert_eq!(normalize_write_allocation_len(4, 128, false), 8);
    assert_eq!(normalize_write_allocation_len(512, 4096, true), 1024);
}

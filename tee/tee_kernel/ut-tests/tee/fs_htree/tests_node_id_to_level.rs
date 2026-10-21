// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::assert_eq;

use super::*;

#[unittest::def_test]
pub fn test_node_id_to_level_basic() {
    assert_eq!(node_id_to_level(1), 1);
    assert_eq!(node_id_to_level(2), 2);
    assert_eq!(node_id_to_level(3), 2);
    assert_eq!(node_id_to_level(4), 3);
    assert_eq!(node_id_to_level(7), 3);
    assert_eq!(node_id_to_level(8), 4);
    assert_eq!(node_id_to_level(15), 4);
    assert_eq!(node_id_to_level(16), 5);
}

#[unittest::def_test]
pub fn test_node_id_to_level_high_values() {
    assert_eq!(node_id_to_level(1023), 10); // 2^10 - 1
    assert_eq!(node_id_to_level(1024), 11); // 2^10
    assert_eq!(node_id_to_level(1 << 30), 31);
}

// Kept for std `#[should_panic]` semantics; not registered via `def_test` because
// kernel unittest cannot catch panics yet.
#[allow(dead_code)]
#[should_panic]
pub(super) fn test_node_id_zero_should_panic() {
    node_id_to_level(0);
}

#[allow(dead_code)]
#[should_panic]
pub(super) fn test_node_id_max_should_panic() {
    node_id_to_level(usize::MAX);
}

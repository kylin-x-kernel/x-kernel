// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::{allocations_in, disable_tracking, enable_tracking, tracking_enabled};

#[def_test]
fn test_tracking_toggle() {
    disable_tracking();
    assert!(!tracking_enabled());
    enable_tracking();
    assert!(tracking_enabled());
    disable_tracking();
    assert!(!tracking_enabled());
}

#[def_test]
fn test_allocations_in_empty() {
    enable_tracking();
    let mut count = 0usize;
    allocations_in(0..0, |_| count += 1);
    assert_eq!(count, 0);
    disable_tracking();
}

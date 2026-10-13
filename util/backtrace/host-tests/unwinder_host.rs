// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use super::*;

#[test]
fn test_unwinder_validates_fp_range() {
    let config = BacktraceConfig::new(0..0x1000, 0..0x1000);
    let unwinder = Unwinder::new(&config);

    // Out of range frame pointer
    let mut buffer = [Frame::new(0, 0); 8];
    assert_eq!(unwinder.unwind(0x2000, &mut buffer), 0);
}

#[test]
fn test_unwinder_stops_at_buffer_capacity() {
    let config = BacktraceConfig::new(0..usize::MAX, 0..usize::MAX);
    let unwinder = Unwinder::new(&config);
    let mut buffer = [Frame::new(0, 0); 2];
    // An invalid frame pointer aborts immediately.
    assert_eq!(unwinder.unwind(0, &mut buffer), 0);
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

#[def_test]
fn windows_grow_on_markers_and_random_misses_preserve_history() {
    let mut ra = FileRaState::new(block::BackingDevInfo::DEFAULT_RA_PAGES);
    assert_eq!(ra.on_miss(0, 1, |_| 0), (0..4, Some(1)));
    assert_eq!(ra.on_miss(100, 1, |_| 0), (100..101, None));
    assert_eq!(ra.on_marker(1, 1, |_| None), Some((4..12, Some(4))));
    assert_eq!(ra.on_marker(4, 1, |_| None), Some((12..28, Some(12))));
    assert_eq!(ra.on_marker(12, 1, |_| None), Some((28..60, Some(28))));
    // Another open's marker reconstructs a window from shared cache history.
    let mut other = FileRaState::new(block::BackingDevInfo::DEFAULT_RA_PAGES);
    assert_eq!(other.on_marker(1, 1, |_| Some(4)), Some((4..12, Some(4))));
    assert_eq!(other.on_miss(100, 1, |_| 8), (100..109, Some(108)));
    ra.shrink_after_io_error();
    assert_eq!(ra.on_miss(0, 32, |_| 0).0, 0..8);
    ra.shrink_after_io_error();
    ra.shrink_after_io_error();
    assert_eq!(ra.on_miss(0, 32, |_| 0), (0..1, None));
    assert_eq!(ra.on_marker(1, 1, |_| Some(4)), None);
}

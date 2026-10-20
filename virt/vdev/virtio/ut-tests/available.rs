// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Available-ring layout regression in the kernel test harness.

use super::*;
use crate::NoGuestMemoryAccessor;

#[unittest::def_test]
fn layout_size_counts_header_entries_and_footer() {
    // header (4) + 4 entries * 2 bytes + used_event footer (2) = 14.
    assert_eq!(AvailableRing::<NoGuestMemoryAccessor>::layout_size(4), 14);
    // Boundary: the largest queue size still counts the footer.
    assert_eq!(
        AvailableRing::<NoGuestMemoryAccessor>::layout_size(256),
        4 + 256 * 2 + 2
    );
}

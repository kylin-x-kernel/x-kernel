// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Used-ring layout regression in the kernel test harness.

use super::*;
use crate::NoGuestMemoryAccessor;

#[unittest::def_test]
fn layout_size_counts_header_elements_and_footer() {
    // header (4) + 4 elements * 8 bytes + avail_event footer (2) = 38.
    assert_eq!(UsedRing::<NoGuestMemoryAccessor>::layout_size(4), 38);
    // Boundary: the largest queue size still counts the footer.
    assert_eq!(
        UsedRing::<NoGuestMemoryAccessor>::layout_size(256),
        4 + 256 * 8 + 2
    );
}

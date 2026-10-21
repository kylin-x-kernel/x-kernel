// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;
use crate::superblock::test_fixtures::{TEST_FREE_BLOCKS, journal_allocator_test_filesystem};

#[def_test]
fn recovery_rejects_reserved_out_of_range_and_unallocated_heads() {
    let (filesystem, _) = journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let first = filesystem.superblock().first_inode();
    let count = filesystem.superblock().inodes_count();
    for number in (1..first).chain(core::iter::once(count.checked_add(1).unwrap())) {
        assert!(
            filesystem
                .recovery_orphan(InodeNumber::new(number), OrphanLocation::LegacyList)
                .unwrap()
                .is_none(),
            "invalid orphan number {number} must not be loaded"
        );
    }
    let unallocated = (first..=count)
        .map(InodeNumber::new)
        .find(|inode| !filesystem.is_inode_allocated(*inode).unwrap())
        .expect("fixture must contain an unallocated inode");
    assert!(
        filesystem
            .recovery_orphan(unallocated, OrphanLocation::LegacyList)
            .unwrap()
            .is_none(),
        "an unallocated inode must not be loaded as an orphan"
    );
}

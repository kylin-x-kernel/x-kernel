// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

#[def_test]
fn inode_allocation_skips_reserved_inodes() {
    let range =
        InodeGroupRange::new(BlockGroupNumber::new(0), InodeNumber::new(1), 16).unwrap();
    let mut bitmap = [0xff, 0x03];

    let allocation =
        allocate_inode_from_bitmap(&mut bitmap, range, None, |inode| inode.get() < 11).unwrap();

    assert_eq!(allocation.group(), BlockGroupNumber::new(0));
    assert_eq!(allocation.inode(), InodeNumber::new(11));
    assert_eq!(allocation.bitmap_bit(), 10);
    assert_eq!(bitmap, [0xff, 0x07]);
}

#[def_test]
fn inode_release_rejects_reserved_and_duplicate_release() {
    let range =
        InodeGroupRange::new(BlockGroupNumber::new(0), InodeNumber::new(1), 16).unwrap();
    let mut bitmap = [0xff, 0x07];

    assert_eq!(
        release_inode_to_bitmap(&mut bitmap, range, InodeNumber::new(2), |inode| {
            inode.get() < 11
        }),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeBitmap))
    );

    let released =
        release_inode_to_bitmap(&mut bitmap, range, InodeNumber::new(11), |_| false).unwrap();
    assert_eq!(released.inode(), InodeNumber::new(11));
    assert_eq!(bitmap, [0xff, 0x03]);
    assert_eq!(
        release_inode_to_bitmap(&mut bitmap, range, InodeNumber::new(11), |_| false),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeBitmap))
    );
}

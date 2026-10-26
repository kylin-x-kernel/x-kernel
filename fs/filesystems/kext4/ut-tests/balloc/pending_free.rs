// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

#[def_test]
fn pending_ranges_merge_clip_and_complete_by_exact_transaction() {
    let mut pending = PendingBlockFrees::default();
    let group = BlockGroupNumber::new(0);
    let old = TransactionId::new(u32::MAX);
    let next = TransactionId::new(0);
    for (first, last) in [
        (10, 265),
        (20, 40),
        (270, 300),
        (260, 275),
        (3, 9),
        (3, 300),
    ] {
        pending
            .insert(
                group,
                old,
                FilesystemBlock::new(first),
                FilesystemBlock::new(last),
            )
            .unwrap();
    }
    assert_eq!(pending.block_count(), 298);
    pending
        .insert(
            group,
            next,
            FilesystemBlock::new(301),
            FilesystemBlock::new(302),
        )
        .unwrap();
    pending
        .insert(
            BlockGroupNumber::new(1),
            old,
            FilesystemBlock::new(512),
            FilesystemBlock::new(513),
        )
        .unwrap();
    assert_eq!(pending.block_count(), 302);
    let mut ranges = Vec::new();
    pending.append_ranges(
        group,
        FilesystemBlock::new(20),
        FilesystemBlock::new(30),
        &mut ranges,
    );
    assert_eq!(
        ranges,
        [(FilesystemBlock::new(20), FilesystemBlock::new(30))]
    );
    pending.complete(old);
    assert_eq!(pending.block_count(), 2);
    assert!(pending.contains_transaction(next));
    assert!(!pending.contains_transaction(old));
    pending.complete(old);
    assert_eq!(pending.block_count(), 2);
    pending.complete(next);
    assert_eq!(pending.block_count(), 0);
    assert!(pending.groups.is_empty());
}

#[def_test]
fn pending_range_rejects_invalid_endpoints_without_count_change() {
    let mut ranges = FreedBlocks::default();
    ranges
        .insert_range(FilesystemBlock::new(3), FilesystemBlock::new(9))
        .unwrap();
    assert_eq!(
        ranges.insert_range(FilesystemBlock::new(9), FilesystemBlock::new(8)),
        Err(Ext4Error::OutOfBounds)
    );
    assert_eq!(
        ranges.insert_range(FilesystemBlock::new(0), FilesystemBlock::new(u64::MAX)),
        Err(Ext4Error::Overflow)
    );
    assert_eq!(ranges.block_count, 7);
}

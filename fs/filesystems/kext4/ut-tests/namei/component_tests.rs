// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Component invariants exercised in the kernel with synthetic metadata.

use unittest::def_test;

use crate::superblock::test_fixtures::eviction_test_unlinked_inode;

#[def_test]
fn eviction_release_batch_partial_extent_split() {
    let (mut filesystem, inode, timestamp) = eviction_test_unlinked_inode(&[(0, 100)]);
    filesystem.eviction_prepare(&inode).unwrap();

    // max_blocks = 40 < 100, so only the tail 40 blocks are released.
    let (freed, done) = filesystem.eviction_release_batch(&inode, 40).unwrap();
    assert_eq!(freed, 40);
    assert!(!done);

    // The remaining extent must keep logical [0, 60).
    let collected = filesystem.collect_extent_tree(&inode).unwrap();
    assert_eq!(collected.extents.len(), 1);
    assert_eq!(collected.extents[0].logical, 0);
    assert_eq!(collected.extents[0].len, 60);

    // Drain the rest under a bounded loop.
    let mut batches = 1;
    loop {
        let (_, done) = filesystem.eviction_release_batch(&inode, 40).unwrap();
        batches += 1;
        assert!(batches <= 10, "eviction must terminate");
        if done {
            break;
        }
    }

    let collected = filesystem.collect_extent_tree(&inode).unwrap();
    assert!(collected.extents.is_empty());
    filesystem.eviction_finish(&inode, timestamp).unwrap();
}

#[def_test]
fn eviction_release_batch_single_batch_empties_tree() {
    // 3 extents of 10 blocks = 30 blocks total, well within max_blocks.
    let (mut filesystem, inode, timestamp) =
        eviction_test_unlinked_inode(&[(0, 10), (10, 10), (20, 10)]);
    filesystem.eviction_prepare(&inode).unwrap();

    // All extents fit in one batch, so the tree is emptied and done = true.
    let (freed, done) = filesystem.eviction_release_batch(&inode, 256).unwrap();
    assert_eq!(freed, 30);
    assert!(done);

    let collected = filesystem.collect_extent_tree(&inode).unwrap();
    assert!(collected.extents.is_empty());
    filesystem.eviction_finish(&inode, timestamp).unwrap();
}

#[def_test]
fn eviction_release_batch_multi_batch_drains_tree() {
    // 10 extents of 10 blocks = 100 blocks, released in max_blocks = 30
    // batches, exercising the keep_count != 0 / keep_count != extents.len()
    // branch across multiple iterations.
    let extents: [(u64, u32); 10] = [
        (0, 10),
        (10, 10),
        (20, 10),
        (30, 10),
        (40, 10),
        (50, 10),
        (60, 10),
        (70, 10),
        (80, 10),
        (90, 10),
    ];
    let (mut filesystem, inode, timestamp) = eviction_test_unlinked_inode(&extents);
    filesystem.eviction_prepare(&inode).unwrap();

    let mut batches = 0;
    loop {
        let (_, done) = filesystem.eviction_release_batch(&inode, 30).unwrap();
        batches += 1;
        assert!(
            batches <= 10,
            "eviction must terminate within bounded batches"
        );
        if done {
            break;
        }
    }
    assert!(
        batches >= 2,
        "expected more than one batch for 100 blocks at 30/batch"
    );

    let collected = filesystem.collect_extent_tree(&inode).unwrap();
    assert!(collected.extents.is_empty());
    filesystem.eviction_finish(&inode, timestamp).unwrap();
}

#[def_test]
fn eviction_release_batch_no_data_blocks_fast_path() {
    // No extents inserted: an unlinked inode with zero data blocks must
    // take the fast path and report (0, true) without touching the tree.
    let (mut filesystem, inode, timestamp) = eviction_test_unlinked_inode(&[]);
    filesystem.eviction_prepare(&inode).unwrap();

    let (freed, done) = filesystem.eviction_release_batch(&inode, 256).unwrap();
    assert_eq!(freed, 0);
    assert!(done);

    filesystem.eviction_finish(&inode, timestamp).unwrap();
}

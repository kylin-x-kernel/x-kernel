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

#[def_test]
fn in_place_namespace_updates_do_not_commit_pending_frees() {
    use crate::{Ext4Timestamp, InodeNumber, jbd2::JournalCredits, superblock::test_fixtures::*};

    let (mut filesystem, device) = recovery_test_filesystem();
    let root = filesystem.internal_iget(InodeNumber::new(2)).unwrap();
    let timestamp = Ext4Timestamp::new(2021, 0);
    let inode = filesystem
        .create_regular_file(&root, b"before", 0o644, 0, 0, timestamp)
        .unwrap();
    let journal = filesystem.metadata_journal().unwrap();
    let mut handle = journal.begin(JournalCredits::new(3)).unwrap();
    let block = filesystem.allocate_block(None, &mut handle).unwrap();
    handle.stop().unwrap();
    filesystem.sync_filesystem().unwrap();
    let mut handle = journal.begin(JournalCredits::new(3)).unwrap();
    filesystem
        .release_allocated_block(block.block(), &mut handle)
        .unwrap();
    let transaction = handle.id();
    handle.stop().unwrap();
    let flushes = device.flush_count();
    filesystem
        .rename(&root, b"before", &inode, &root, b"after", None, timestamp)
        .unwrap();
    filesystem
        .unlink(&root, b"after", &inode, timestamp)
        .unwrap();
    assert_eq!(device.flush_count(), flushes);
    assert_eq!(
        journal.begin(JournalCredits::new(0)).unwrap().id(),
        transaction
    );
    filesystem.eviction_prepare(&inode).unwrap();
    filesystem.eviction_finish(&inode, timestamp).unwrap();
    filesystem.sync_filesystem().unwrap();
}

#[def_test]
fn eviction_phases_share_transaction_until_sync() {
    let (mut filesystem, inode, timestamp) = eviction_test_unlinked_inode(&[(0, 100)]);
    crate::superblock::test_fixtures::install_credit_limited_journal(
        &mut filesystem,
        2001,
        1000,
        true,
    );
    filesystem.eviction_prepare(&inode).unwrap();
    let journal = filesystem.metadata_journal().unwrap();
    let transaction = journal
        .begin(crate::jbd2::JournalCredits::new(0))
        .unwrap()
        .id();
    for batch in 0..4 {
        inode.enable_extra_isize_expansion();
        let (_, done) = filesystem.eviction_release_batch(&inode, 30).unwrap();
        assert_eq!(done, batch == 3);
        assert!(inode.is_extra_isize_expansion_disabled());
        assert_eq!(
            journal
                .begin(crate::jbd2::JournalCredits::new(0))
                .unwrap()
                .id(),
            transaction
        );
        assert_eq!(filesystem.pending_checkpoint_count(), 0);
    }
    filesystem.eviction_finish(&inode, timestamp).unwrap();
    assert_eq!(
        journal
            .begin(crate::jbd2::JournalCredits::new(0))
            .unwrap()
            .id(),
        transaction
    );
    assert!(!filesystem.is_inode_allocated(inode.number()).unwrap());
    filesystem.sync_filesystem().unwrap();
    assert!(!filesystem.commit_running_metadata_transaction().unwrap());
    assert_eq!(filesystem.pending_checkpoint_count(), 0);
}

#[def_test]
fn external_leaf_tail_batches_share_transaction() {
    let (mut filesystem, inode, timestamp) =
        eviction_test_unlinked_inode(&[(0, 1), (2, 1), (4, 1), (6, 1), (8, 1), (10, 64)]);
    crate::superblock::test_fixtures::install_credit_limited_journal(
        &mut filesystem,
        2001,
        1000,
        true,
    );
    filesystem.eviction_prepare(&inode).unwrap();
    let journal = filesystem.metadata_journal().unwrap();
    let transaction = journal
        .begin(crate::jbd2::JournalCredits::new(0))
        .unwrap()
        .id();
    let mut released = 0;
    loop {
        let (freed, done) = filesystem.eviction_release_batch(&inode, 16).unwrap();
        assert!(freed > 0 && freed <= 16);
        released += freed;
        assert_eq!(
            journal
                .begin(crate::jbd2::JournalCredits::new(0))
                .unwrap()
                .id(),
            transaction
        );
        assert_eq!(filesystem.pending_checkpoint_count(), 0);
        if done {
            break;
        }
    }
    assert_eq!(released, 69);
    filesystem.eviction_finish(&inode, timestamp).unwrap();
    filesystem.sync_filesystem().unwrap();
}

#[def_test]
fn eviction_prunes_multiple_leaves_without_rebuilding_or_allocating() {
    let extents: alloc::vec::Vec<_> = (0..200).map(|index| (index * 2, 1)).collect();
    let (mut filesystem, inode, timestamp) = eviction_test_unlinked_inode(&extents);
    // Populate the mapping cache before pruning the rightmost leaf.
    assert!(matches!(
        filesystem
            .map_blocks(&inode, crate::LogicalBlock::new(398))
            .unwrap(),
        crate::BlockMapping::Mapped { .. }
    ));
    crate::superblock::test_fixtures::install_credit_limited_journal(
        &mut filesystem,
        2001,
        1000,
        true,
    );
    filesystem.eviction_prepare(&inode).unwrap();
    let journal = filesystem.metadata_journal().unwrap();
    let transaction = journal
        .begin(crate::jbd2::JournalCredits::new(0))
        .unwrap()
        .id();
    let mut released = 0;
    let mut free = filesystem.free_blocks_count();
    loop {
        let (freed, done) = filesystem.eviction_release_batch(&inode, 32).unwrap();
        assert!(freed > 0 && freed <= 32);
        assert!(filesystem.free_blocks_count() >= free + u64::from(freed));
        free = filesystem.free_blocks_count();
        released += freed;
        assert_eq!(
            journal
                .begin(crate::jbd2::JournalCredits::new(0))
                .unwrap()
                .id(),
            transaction
        );
        assert_eq!(filesystem.pending_checkpoint_count(), 0);
        assert!(matches!(
            filesystem
                .map_blocks(&inode, crate::LogicalBlock::new(398))
                .unwrap(),
            crate::BlockMapping::Hole { .. }
        ));
        if done {
            break;
        }
    }
    assert_eq!(released, 200);
    assert_eq!(inode.blocks(), 0);
    filesystem.eviction_finish(&inode, timestamp).unwrap();
    filesystem.sync_filesystem().unwrap();
}

#[def_test]
fn eviction_recovers_at_each_phase_before_and_after_commit() {
    use alloc::{sync::Arc, vec};

    use crate::{
        Ext4SbInfo, Ext4SyncIntent, Ext4Timestamp, InodeNumber, superblock::test_fixtures::*,
    };

    for phase in 0..3 {
        for commit in [false, true] {
            let (mut filesystem, device) = recovery_test_filesystem();
            let free_blocks = filesystem.free_blocks_count();
            let free_inodes = filesystem.free_inodes_count();
            let root = filesystem.internal_iget(InodeNumber::new(2)).unwrap();
            let timestamp = Ext4Timestamp::new(2020, 0);
            let inode = filesystem
                .create_regular_file(&root, b"evict-crash", 0o644, 0, 0, timestamp)
                .unwrap();
            let input = vec![0x5a; 16 * 1024];
            inode.set_size(input.len() as u64);
            filesystem
                .writeback_ordered_at(
                    &inode,
                    0,
                    &input,
                    input.len() as u64,
                    timestamp,
                    Ext4SyncIntent::FullMetadata,
                )
                .unwrap();
            filesystem
                .unlink(&root, b"evict-crash", &inode, timestamp)
                .unwrap();
            filesystem.sync_filesystem().unwrap();
            filesystem.eviction_prepare(&inode).unwrap();
            if phase >= 1 {
                assert!(!filesystem.eviction_release_batch(&inode, 2).unwrap().1);
            }
            if phase >= 2 {
                assert!(filesystem.eviction_release_batch(&inode, 256).unwrap().1);
                filesystem.eviction_finish(&inode, timestamp).unwrap();
            }
            if commit {
                assert!(filesystem.commit_running_metadata_transaction().unwrap());
            }
            // Copy only device bytes, dropping all runtime buffer/allocator
            // state. This covers a stopped or committed handle, not torn I/O.
            let crashed = crate::io::test_block_device(Arc::new(TestDevice::new(device.bytes())));
            Ext4SbInfo::recover(crashed.clone()).unwrap();
            let recovered = open_test_filesystem(crashed).unwrap();
            assert!(!recovered.is_inode_allocated(inode.number()).unwrap());
            assert_eq!(recovered.free_blocks_count(), free_blocks);
            assert_eq!(recovered.free_inodes_count(), free_inodes);
        }
    }
}

#[def_test]
fn eviction_admission_failure_does_not_disable_extra_isize_expansion() {
    let (mut filesystem, inode, _) = eviction_test_unlinked_inode(&[]);
    crate::superblock::test_fixtures::install_credit_limited_journal(
        &mut filesystem,
        2002,
        1,
        true,
    );
    assert!(!inode.is_extra_isize_expansion_disabled());
    assert!(filesystem.eviction_prepare(&inode).is_err());
    assert!(!inode.is_extra_isize_expansion_disabled());
}

#[def_test]
fn directory_eviction_shrinks_batches_to_fit_revoke_capacity() {
    let (mut filesystem, inode, timestamp) =
        crate::superblock::test_fixtures::eviction_test_unlinked_inode_with_initialization(
            &[(0, 64)],
            crate::inode::InodeInitialization::directory(0o755, 0, 0),
        );
    crate::superblock::test_fixtures::install_credit_limited_journal(
        &mut filesystem,
        2001,
        64,
        true,
    );
    filesystem.eviction_prepare(&inode).unwrap();
    let (freed, done) = filesystem.eviction_release_batch(&inode, 64).unwrap();
    assert!(freed > 0 && freed < 64);
    assert!(!done);
    let mut total = freed;
    while total < 64 {
        let (freed, done) = filesystem.eviction_release_batch(&inode, 64).unwrap();
        assert!(freed > 0);
        total += freed;
        assert_eq!(done, total == 64);
    }
    filesystem.eviction_finish(&inode, timestamp).unwrap();
    filesystem.sync_filesystem().unwrap();
}

#[def_test]
fn eviction_rejects_capacity_below_smallest_batch_without_mutation() {
    let (mut filesystem, inode, _) = eviction_test_unlinked_inode(&[(0, 64)]);
    crate::superblock::test_fixtures::install_credit_limited_journal(
        &mut filesystem,
        2001,
        1,
        true,
    );
    let blocks = inode.blocks();
    let free = filesystem.free_blocks_count();
    assert_eq!(
        filesystem.eviction_release_batch(&inode, 64),
        Err(crate::Ext4Error::NoSpace)
    );
    assert_eq!(inode.blocks(), blocks);
    assert_eq!(filesystem.free_blocks_count(), free);
    assert!(!filesystem.has_pending_block_frees().unwrap());
    assert!(!filesystem.metadata_journal().unwrap().is_aborted());
}

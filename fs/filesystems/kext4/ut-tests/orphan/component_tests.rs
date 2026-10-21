// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Component invariants exercised in the kernel with synthetic metadata.

use alloc::vec;

use unittest::def_test;

use crate::{
    BlockCount, Ext4Error, Ext4SyncIntent, LogicalBlock,
    disk::features,
    file::RegularWriteMetadata,
    jbd2::{JournalCredits, JournalTransactions, TransactionId},
    superblock::test_fixtures::{
        TEST_BLOCK_SIZE, TEST_FREE_BLOCKS, allocate_checkpointed_regular_inode,
        allocator_test_filesystem, install_test_internal_journal,
        journal_allocator_test_filesystem, set_allocator_feature_bits,
        update_test_inode_links_count,
    },
};

#[def_test]
fn removing_orphan_tail_does_not_restore_stale_predecessor_next() {
    let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let first = allocate_checkpointed_regular_inode(&mut filesystem);
    let second = allocate_checkpointed_regular_inode(&mut filesystem);
    let journal = JournalTransactions::new(TransactionId::new(390));
    let mut handle = journal.begin(JournalCredits::new(32)).unwrap();
    let transaction = handle.id();

    filesystem.add_orphan(&first, &mut handle).unwrap();
    filesystem.add_orphan(&second, &mut handle).unwrap();
    assert_eq!(filesystem.orphan_head(), Some(second.number()));
    assert_eq!(
        filesystem.raw_inode(second.number()).unwrap().dtime(),
        first.number().get()
    );

    filesystem.remove_orphan(&first, &mut handle).unwrap();
    assert_eq!(filesystem.orphan_head(), Some(second.number()));
    assert_eq!(filesystem.raw_inode(second.number()).unwrap().dtime(), 0);

    filesystem.remove_orphan(&second, &mut handle).unwrap();
    assert_eq!(filesystem.orphan_head(), None);

    drop(handle);
    let commit = journal.force_commit(transaction).unwrap();
    filesystem
        .metadata_io
        .checkpoint_committed(&commit)
        .unwrap();
    journal.finish_checkpoint_for_test(&commit).unwrap();
}

#[def_test]
fn orphan_cleanup_finishes_committed_shrink_after_crash_point() {
    let (mut filesystem, _device) =
        journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = allocate_checkpointed_regular_inode(&mut filesystem);
    install_test_internal_journal(&mut filesystem, 751);
    let input = vec![0x5c; TEST_BLOCK_SIZE * 3];
    inode.set_size(input.len() as u64);
    filesystem
        .writeback_ordered_at(
            &inode,
            0,
            &input,
            input.len() as u64,
            crate::Ext4Timestamp::new(31, 0),
            Ext4SyncIntent::FullMetadata,
        )
        .unwrap();
    assert_eq!(inode.blocks(), (TEST_BLOCK_SIZE / 512 * 3) as u64);
    install_test_internal_journal(&mut filesystem, 752);
    let free_before_orphan_cleanup = filesystem.free_blocks_count();
    let new_size = u64::try_from(TEST_BLOCK_SIZE + 9).unwrap();

    let journal = JournalTransactions::new(TransactionId::new(753));
    let mut handle = journal.begin(JournalCredits::new(16)).unwrap();
    let transaction = handle.id();
    filesystem.add_orphan(&inode, &mut handle).unwrap();
    filesystem
        .update_regular_inode_size_metadata(
            &inode,
            new_size,
            RegularWriteMetadata::Full {
                timestamp: crate::Ext4Timestamp::new(32, 0),
            },
            &mut handle,
        )
        .unwrap();
    inode.set_size(new_size);
    drop(handle);
    let commit = journal.force_commit(transaction).unwrap();
    filesystem
        .metadata_io
        .checkpoint_committed(&commit)
        .unwrap();
    journal.finish_checkpoint_for_test(&commit).unwrap();

    assert_eq!(filesystem.orphan_head(), Some(inode.number()));
    assert_eq!(inode.size(), new_size);

    assert_eq!(filesystem.cleanup_orphans().unwrap(), 1);

    let recovered = filesystem.internal_iget(inode.number()).unwrap();
    assert_eq!(recovered.size(), new_size);
    assert_eq!(recovered.blocks(), (TEST_BLOCK_SIZE / 512 * 2) as u64);
    assert_eq!(filesystem.orphan_head(), None);
    assert_eq!(
        filesystem.map_blocks(&recovered, LogicalBlock::new(2)),
        Ok(crate::BlockMapping::Hole {
            len: BlockCount::new(u32::MAX),
            flags: crate::BlockMappingFlags::empty(),
        })
    );
    assert_eq!(
        filesystem.free_blocks_count(),
        free_before_orphan_cleanup + 1
    );
}

#[def_test]
fn cleanup_orphans_rejects_orphan_file_pending_feature() {
    let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    set_allocator_feature_bits(
        &mut filesystem,
        features::CompatFeatures::empty(),
        features::ReadOnlyCompatFeatures::ORPHAN_PRESENT,
    );

    assert_eq!(
        filesystem.cleanup_orphans(),
        Err(Ext4Error::Corrupt(crate::CorruptKind::InvalidOrphanFile))
    );
}

#[def_test]
fn legacy_orphan_cleanup_preserving_recovery_evicts_zero_link_inode() {
    let (mut filesystem, _device) =
        journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = allocate_checkpointed_regular_inode(&mut filesystem);
    install_test_internal_journal(&mut filesystem, 769);
    let free_inodes_before_cleanup = filesystem.free_inodes_count();
    let journal = JournalTransactions::new(TransactionId::new(770));
    let mut handle = journal.begin(JournalCredits::new(8)).unwrap();
    let transaction = handle.id();
    filesystem.add_orphan(&inode, &mut handle).unwrap();
    update_test_inode_links_count(&mut filesystem, &inode, 0, &mut handle);
    drop(handle);
    let commit = journal.force_commit(transaction).unwrap();
    filesystem
        .metadata_io
        .checkpoint_committed(&commit)
        .unwrap();
    journal.finish_checkpoint_for_test(&commit).unwrap();

    let recovery_inode = filesystem
        .recovery_orphan(inode.number(), super::OrphanLocation::LegacyList)
        .unwrap()
        .unwrap();
    filesystem
        .cleanup_unlinked_orphan(
            &recovery_inode,
            crate::journal::RecoveryFlagPolicy::PreserveDuringRecovery,
        )
        .unwrap();
    assert_eq!(filesystem.orphan_head(), None);
    assert!(filesystem.needs_recovery());
    assert_eq!(
        filesystem.free_inodes_count(),
        free_inodes_before_cleanup + 1
    );
}

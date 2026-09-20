// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Component invariants exercised in the kernel with synthetic metadata.

use alloc::vec;

use block::DriverError;
use unittest::def_test;

use crate::{
    BlockCount, BlockMapping, BlockMappingFlags, Ext4Error, Ext4SyncIntent, LogicalBlock,
    PhysicalBlock, UnsupportedKind,
    disk::features,
    extent::ExtentMappingState,
    file::RegularWriteMetadata,
    inode::InodeInitialization,
    jbd2::{JournalCredits, JournalTransactions, TransactionId},
    superblock::test_fixtures::{
        TEST_BLOCK_SIZE, TEST_FREE_BLOCKS, allocate_checkpointed_regular_inode,
        allocate_contiguous_blocks, allocator_test_filesystem,
        fragment_ordered_writeback_free_space, install_credit_limited_journal,
        install_test_internal_journal, journal_allocator_test_filesystem,
        set_allocator_feature_bits,
    },
};

#[def_test]
fn delayed_allocation_range_updates_inode_and_mount_accounting_together() {
    let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = allocate_checkpointed_regular_inode(&mut filesystem);
    let free_before = filesystem.statfs().unwrap().blocks_free;

    filesystem
        .reserve_delalloc_range(&inode, LogicalBlock::new(0), 4)
        .unwrap();
    filesystem
        .reserve_delalloc_range(&inode, LogicalBlock::new(2), 4)
        .unwrap();
    assert_eq!(filesystem.delalloc_reserved_block_count(), 6);
    assert_eq!(filesystem.statfs().unwrap().blocks_free, free_before - 6);
    assert_eq!(
        filesystem
            .report_mapping(&inode, LogicalBlock::new(0))
            .unwrap(),
        BlockMapping::Hole {
            len: BlockCount::new(6),
            flags: BlockMappingFlags::DELAYED,
        }
    );

    filesystem
        .release_delalloc_range(&inode, LogicalBlock::new(1), 4)
        .unwrap();
    assert_eq!(filesystem.delalloc_reserved_block_count(), 2);
    assert_eq!(
        filesystem
            .report_mapping(&inode, LogicalBlock::new(1))
            .unwrap(),
        BlockMapping::Hole {
            len: BlockCount::new(4),
            flags: BlockMappingFlags::empty(),
        }
    );
    filesystem
        .truncate_delalloc_range(&inode, LogicalBlock::new(5))
        .unwrap();
    assert_eq!(filesystem.delalloc_reserved_block_count(), 1);
    filesystem.release_all_delalloc(&inode).unwrap();
    assert_eq!(filesystem.delalloc_reserved_block_count(), 0);
    assert!(!inode.has_delalloc_reservations());
}

#[def_test]
fn delayed_allocation_reuses_existing_reservations_when_space_is_exhausted() {
    let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = allocate_checkpointed_regular_inode(&mut filesystem);
    let available = filesystem.statfs().unwrap().blocks_available;
    assert!(available > 0);
    filesystem
        .reserve_delalloc_range(&inode, LogicalBlock::new(0), available)
        .unwrap();
    assert_eq!(filesystem.statfs().unwrap().blocks_available, 0);

    filesystem
        .reserve_delalloc_range(&inode, LogicalBlock::new(0), available)
        .unwrap();
    assert_eq!(filesystem.delalloc_reserved_block_count(), available);
    assert_eq!(
        filesystem.reserve_delalloc_range(&inode, LogicalBlock::new(available), 1),
        Err(Ext4Error::NoSpace)
    );
    assert_eq!(filesystem.delalloc_reserved_block_count(), available);
    filesystem.release_all_delalloc(&inode).unwrap();
    assert_eq!(filesystem.delalloc_reserved_block_count(), 0);
    assert!(!inode.has_delalloc_reservations());
}

#[def_test]
fn delayed_allocation_only_reserves_holes_between_allocated_blocks() {
    for mapping_state in [
        ExtentMappingState::Initialized,
        ExtentMappingState::Unwritten,
    ] {
        let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        let inode = allocate_checkpointed_regular_inode(&mut filesystem);
        let journal = JournalTransactions::new(TransactionId::new(389));
        let mut handle = journal.begin(JournalCredits::new(256)).unwrap();
        let physical = allocate_contiguous_blocks(&mut filesystem, 2, &mut handle);
        filesystem
            .insert_extent_mapping(
                &inode,
                LogicalBlock::new(1),
                physical,
                BlockCount::new(2),
                mapping_state,
                &mut handle,
            )
            .unwrap();
        filesystem
            .update_inode_size_metadata(
                &inode,
                4 * TEST_BLOCK_SIZE as u64,
                crate::Ext4Timestamp::new(9, 0),
                &mut handle,
            )
            .unwrap();
        let free_before = filesystem.statfs().unwrap().blocks_free;
        let mapping_before = filesystem.map_blocks(&inode, LogicalBlock::new(1)).unwrap();

        filesystem
            .reserve_delalloc_range(&inode, LogicalBlock::new(1), 2)
            .unwrap();
        assert!(!inode.has_delalloc_reservations());
        assert_eq!(filesystem.delalloc_reserved_block_count(), 0);
        assert_eq!(filesystem.statfs().unwrap().blocks_free, free_before);

        filesystem
            .reserve_delalloc_range(&inode, LogicalBlock::new(0), 4)
            .unwrap();
        assert_eq!(filesystem.delalloc_reserved_block_count(), 2);
        assert_eq!(filesystem.statfs().unwrap().blocks_free, free_before - 2);
        filesystem
            .reserve_delalloc_range(&inode, LogicalBlock::new(0), 4)
            .unwrap();
        assert_eq!(filesystem.delalloc_reserved_block_count(), 2);
        assert_eq!(
            filesystem.map_blocks(&inode, LogicalBlock::new(1)).unwrap(),
            mapping_before
        );
        filesystem.release_all_delalloc(&inode).unwrap();
        assert_eq!(filesystem.delalloc_reserved_block_count(), 0);
        assert_eq!(filesystem.statfs().unwrap().blocks_free, free_before);
        assert!(!inode.has_delalloc_reservations());

        // Prime the overwrite cache, then turn part of that range into a
        // hole. A stale proof would incorrectly skip this reservation.
        filesystem
            .reserve_delalloc_range(&inode, LogicalBlock::new(1), 2)
            .unwrap();
        filesystem
            .remove_extent_range(
                &inode,
                LogicalBlock::new(1),
                BlockCount::new(1),
                &mut handle,
            )
            .unwrap();
        filesystem
            .reserve_delalloc_range(&inode, LogicalBlock::new(1), 2)
            .unwrap();
        assert_eq!(filesystem.delalloc_reserved_block_count(), 1);
        assert!(matches!(
            filesystem.report_mapping(&inode, LogicalBlock::new(1)),
            Ok(BlockMapping::Hole {
                flags: BlockMappingFlags::DELAYED,
                ..
            })
        ));
        filesystem.release_all_delalloc(&inode).unwrap();
        assert_eq!(filesystem.delalloc_reserved_block_count(), 0);
    }
}

#[def_test]
fn delayed_allocation_rejects_legacy_inode_before_reserving_space() {
    let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = allocate_checkpointed_regular_inode(&mut filesystem);
    let journal = JournalTransactions::new(TransactionId::new(390));
    let mut handle = journal.begin(JournalCredits::new(1)).unwrap();
    let transaction = handle.id();
    filesystem
        .update_inode_flags_timestamps_metadata(
            &inode,
            inode.flags() & !crate::disk::inode::EXT4_EXTENTS_FL,
            crate::Ext4Timestamp::new(9, 0),
            &mut handle,
        )
        .expect("prepare an extents-enabled filesystem containing one legacy inode");
    drop(handle);
    let commit = journal.force_commit(transaction).unwrap();
    filesystem
        .metadata_io
        .checkpoint_committed(&commit)
        .unwrap();
    journal.finish_checkpoint_for_test(&commit).unwrap();
    assert!(!inode.has_extents());

    assert_eq!(
        filesystem.reserve_delalloc_range(&inode, LogicalBlock::new(0), 1),
        Err(Ext4Error::Unsupported(UnsupportedKind::NonExtentInode))
    );
    assert!(!inode.has_delalloc_reservations());
    assert_eq!(filesystem.delalloc_reserved_block_count(), 0);
}

#[def_test]
fn ordered_writeback_commits_disk_size_without_changing_visible_size() {
    let (mut filesystem, _device) =
        journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = allocate_checkpointed_regular_inode(&mut filesystem);
    install_test_internal_journal(&mut filesystem, 701);
    let input = vec![0x5a; TEST_BLOCK_SIZE];
    let visible_size = u64::try_from(TEST_BLOCK_SIZE * 2).unwrap();
    let timestamp = crate::Ext4Timestamp::new(1234, 0);

    inode.set_size(visible_size);
    filesystem
        .writeback_ordered_at(
            &inode,
            0,
            &input,
            visible_size,
            timestamp,
            Ext4SyncIntent::DataOnly,
        )
        .unwrap();

    assert_eq!(inode.size(), visible_size);
    assert_eq!(inode.disk_size(), TEST_BLOCK_SIZE as u64);
    assert_eq!(inode.blocks(), (TEST_BLOCK_SIZE / 512) as u64);
    assert_ne!(inode.ctime(), timestamp);
    assert_ne!(inode.mtime(), timestamp);
    assert_eq!(
        filesystem.map_blocks(&inode, LogicalBlock::new(0)),
        Ok(crate::BlockMapping::Mapped {
            physical: PhysicalBlock::new(6),
            len: BlockCount::new(1),
            flags: crate::BlockMappingFlags::empty(),
        })
    );
    let mut output = vec![0; TEST_BLOCK_SIZE];
    assert_eq!(
        filesystem.read_at(&inode, 0, &mut output).unwrap(),
        TEST_BLOCK_SIZE
    );
    assert_eq!(output, input);

    filesystem
        .commit_regular_inode_write_metadata(
            &inode,
            visible_size,
            RegularWriteMetadata::Full { timestamp },
        )
        .unwrap();
    assert_eq!(inode.size(), visible_size);
    assert_eq!(inode.blocks(), (TEST_BLOCK_SIZE / 512) as u64);
    assert_eq!(inode.ctime(), timestamp);
    assert_eq!(inode.mtime(), timestamp);
    assert_eq!(
        filesystem.commit_regular_inode_write_metadata(
            &inode,
            visible_size - 1,
            RegularWriteMetadata::SizeOnly,
        ),
        Err(Ext4Error::Unsupported(UnsupportedKind::FileSizeShrink))
    );
    let mut sparse_tail = [0xff];
    assert_eq!(
        filesystem
            .read_at(&inode, visible_size - 1, &mut sparse_tail)
            .unwrap(),
        1
    );
    assert_eq!(sparse_tail, [0]);
}

#[def_test]
fn ordered_writeback_append_preserves_existing_partial_block_data() {
    let (mut filesystem, _device) =
        journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = allocate_checkpointed_regular_inode(&mut filesystem);
    install_test_internal_journal(&mut filesystem, 711);
    let timestamp = crate::Ext4Timestamp::new(5678, 0);

    inode.set_size(3);
    filesystem
        .writeback_ordered_at(&inode, 0, b"abc", 3, timestamp, Ext4SyncIntent::DataOnly)
        .unwrap();
    inode.set_size(6);
    filesystem
        .writeback_ordered_at(&inode, 3, b"def", 6, timestamp, Ext4SyncIntent::DataOnly)
        .unwrap();

    let mut output = vec![0; 6];
    assert_eq!(filesystem.read_at(&inode, 0, &mut output).unwrap(), 6);
    assert_eq!(&output, b"abcdef");
    assert_eq!(inode.size(), 6);
}

#[def_test]
fn ordered_writeback_preallocates_and_discards_unwritten_tail() {
    let (mut filesystem, _device) =
        journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = allocate_checkpointed_regular_inode(&mut filesystem);
    install_test_internal_journal(&mut filesystem, 712);
    let input = vec![0x42; TEST_BLOCK_SIZE * 4];
    let free_before_write = filesystem.free_blocks_count();

    inode.set_size(input.len() as u64);
    filesystem
        .writeback_ordered_at(
            &inode,
            0,
            &input,
            input.len() as u64,
            crate::Ext4Timestamp::new(5680, 0),
            Ext4SyncIntent::DataOnly,
        )
        .unwrap();

    assert_eq!(inode.size(), input.len() as u64);
    assert_eq!(inode.blocks(), (TEST_BLOCK_SIZE / 512 * 8) as u64);
    assert_eq!(
        filesystem.map_blocks(&inode, LogicalBlock::new(0)),
        Ok(crate::BlockMapping::Mapped {
            physical: PhysicalBlock::new(6),
            len: BlockCount::new(4),
            flags: crate::BlockMappingFlags::empty(),
        })
    );
    assert_eq!(
        filesystem.map_blocks(&inode, LogicalBlock::new(4)),
        Ok(crate::BlockMapping::Unwritten {
            physical: PhysicalBlock::new(10),
            len: BlockCount::new(4),
            flags: crate::BlockMappingFlags::empty(),
        })
    );
    assert_eq!(filesystem.free_blocks_count(), free_before_write - 8);
    assert!(
        filesystem
            .extent_truncate_metadata_credits(&inode, LogicalBlock::new(4))
            .unwrap()
            <= 16,
        "discard credits must follow the mapped tail structure, not total inode blocks"
    );

    install_test_internal_journal(&mut filesystem, 713);
    filesystem
        .discard_regular_inode_preallocations(&inode)
        .unwrap();

    assert_eq!(inode.blocks(), (TEST_BLOCK_SIZE / 512 * 4) as u64);
    assert_eq!(
        filesystem.map_blocks(&inode, LogicalBlock::new(4)),
        Ok(crate::BlockMapping::Hole {
            len: BlockCount::new(u32::MAX),
            flags: crate::BlockMappingFlags::empty(),
        })
    );
    assert_eq!(filesystem.free_blocks_count(), free_before_write - 4);
}

#[def_test]
fn ordered_writeback_prealloc_budget_zero_allocates_only_dirty_blocks() {
    let (mut filesystem, _device) =
        journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = allocate_checkpointed_regular_inode(&mut filesystem);
    install_test_internal_journal(&mut filesystem, 713);
    let input = vec![0x34; TEST_BLOCK_SIZE * 4];
    let free_before_write = filesystem.free_blocks_count();

    inode.set_size(input.len() as u64);
    filesystem
        .writeback_ordered_at_with_prealloc_budget(
            &inode,
            0,
            &input,
            input.len() as u64,
            crate::Ext4Timestamp::new(5683, 0),
            Ext4SyncIntent::DataOnly,
            0,
        )
        .unwrap();

    assert_eq!(inode.blocks(), (TEST_BLOCK_SIZE / 512 * 4) as u64);
    assert_eq!(
        filesystem.map_blocks(&inode, LogicalBlock::new(0)),
        Ok(crate::BlockMapping::Mapped {
            physical: PhysicalBlock::new(6),
            len: BlockCount::new(4),
            flags: crate::BlockMappingFlags::empty(),
        })
    );
    assert_eq!(
        filesystem.map_blocks(&inode, LogicalBlock::new(4)),
        Ok(crate::BlockMapping::Hole {
            len: BlockCount::new(u32::MAX),
            flags: crate::BlockMappingFlags::empty(),
        })
    );
    assert_eq!(filesystem.free_blocks_count(), free_before_write - 4);
}

#[def_test]
fn ordered_writeback_restarts_transaction_for_actual_allocation_runs() {
    let (mut filesystem, device) = journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = allocate_checkpointed_regular_inode(&mut filesystem);
    fragment_ordered_writeback_free_space(&mut filesystem);
    install_credit_limited_journal(&mut filesystem, 714, 79, true);
    drop(filesystem.metadata_journal().unwrap());
    let input = vec![0x7a; TEST_BLOCK_SIZE * 6];
    let flushes_before = device.flush_count();

    inode.set_size(input.len() as u64);
    filesystem
        .reserve_delalloc_range(&inode, LogicalBlock::new(0), 6)
        .unwrap();
    filesystem
        .writeback_ordered_at_with_prealloc_budget(
            &inode,
            0,
            &input,
            input.len() as u64,
            crate::Ext4Timestamp::new(5684, 0),
            Ext4SyncIntent::DataOnly,
            0,
        )
        .expect("restart after the actual allocation runs exhaust one transaction");

    // The test leaves only single-block free runs and limits a transaction
    // to 79 credits. The fifth insertion grows the inline extent root;
    // extending for the sixth run then commits that prefix and restarts.
    assert_eq!(filesystem.pending_checkpoint_count(), 1);
    // Data, recovery evidence, journal activation, commit, then tail data.
    assert_eq!(device.flush_count(), flushes_before + 5);
    assert_eq!(inode.disk_size(), input.len() as u64);
    assert!(!inode.has_delalloc_reservations());
    assert_eq!(filesystem.delalloc_reserved_blocks(), 0);
    for logical in 0..6u64 {
        assert!(matches!(
            filesystem.map_blocks(&inode, LogicalBlock::new(logical)),
            Ok(crate::BlockMapping::Mapped { .. })
        ));
    }
    let mut output = vec![0; input.len()];
    assert_eq!(filesystem.read_at(&inode, 0, &mut output), Ok(input.len()));
    assert_eq!(output, input);
}

#[def_test]
fn ordered_writeback_preserves_prealloc_budget_across_five_restarts() {
    let (mut filesystem, _device) =
        journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let setup_journal = JournalTransactions::new(TransactionId::new(714));
    let mut handle = setup_journal.begin(JournalCredits::new(1024)).unwrap();
    let transaction = handle.id();
    let allocation = filesystem
        .allocate_inode(
            None,
            InodeInitialization::regular_file(0o644, 0, 0),
            &mut handle,
        )
        .unwrap();
    let inode = filesystem.internal_iget(allocation.inode()).unwrap();
    let physical = allocate_contiguous_blocks(&mut filesystem, 5, &mut handle);
    for (index, logical) in [1u64, 3, 5, 7, 9].into_iter().enumerate() {
        filesystem
            .insert_extent_mapping(
                &inode,
                LogicalBlock::new(logical),
                PhysicalBlock::new(physical.get() + u64::try_from(index).unwrap()),
                BlockCount::new(1),
                ExtentMappingState::Initialized,
                &mut handle,
            )
            .unwrap();
    }
    drop(handle);
    let commit = setup_journal.force_commit(transaction).unwrap();
    filesystem
        .metadata_io
        .checkpoint_committed(&commit)
        .unwrap();
    setup_journal.finish_checkpoint_for_test(&commit).unwrap();

    // A 75-credit transaction: one hole mutation fits,
    // but extending the handle for the next hole must restart. The five
    // one-block holes are separated by mapped runs; the final four-block
    // tail consumes the optional four-block preallocation budget once.
    install_credit_limited_journal(&mut filesystem, 715, 75, true);
    let input = vec![0x6b; TEST_BLOCK_SIZE * 14];
    let free_before_write = filesystem.free_blocks_count();
    let blocks_before_write = inode.blocks();
    inode.set_size(input.len() as u64);
    filesystem
        .reserve_delalloc_range(&inode, LogicalBlock::new(0), 14)
        .unwrap();

    filesystem
        .writeback_ordered_at_with_prealloc_budget(
            &inode,
            0,
            &input,
            input.len() as u64,
            crate::Ext4Timestamp::new(5687, 0),
            Ext4SyncIntent::DataOnly,
            4,
        )
        .expect("five transaction restarts preserve the one-shot preallocation budget");

    assert_eq!(filesystem.pending_checkpoint_count(), 5);
    assert_eq!(inode.disk_size(), input.len() as u64);
    assert!(!inode.has_delalloc_reservations());
    assert_eq!(filesystem.delalloc_reserved_blocks(), 0);
    // i_blocks includes the extent leaf already allocated by setup.
    assert_eq!(
        inode.blocks(),
        blocks_before_write + u64::try_from(13 * (TEST_BLOCK_SIZE / 512)).unwrap()
    );
    assert_eq!(filesystem.free_blocks_count(), free_before_write - 13);
    let Ok(crate::BlockMapping::Mapped {
        physical: dirty_tail,
        len: dirty_tail_len,
        ..
    }) = filesystem.map_blocks(&inode, LogicalBlock::new(10))
    else {
        panic!("dirty tail must be mapped after writeback");
    };
    assert_eq!(dirty_tail_len, BlockCount::new(4));
    assert_eq!(
        filesystem.map_blocks(&inode, LogicalBlock::new(14)),
        Ok(crate::BlockMapping::Unwritten {
            physical: PhysicalBlock::new(dirty_tail.get() + 4),
            len: BlockCount::new(4),
            flags: crate::BlockMappingFlags::empty(),
        })
    );
    let mut output = vec![0; input.len()];
    assert_eq!(filesystem.read_at(&inode, 0, &mut output), Ok(input.len()));
    assert_eq!(output, input);
}

#[def_test]
fn ordered_writeback_restarts_across_hole_mapped_and_unwritten_runs() {
    let (mut filesystem, _device) =
        journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let setup_journal = JournalTransactions::new(TransactionId::new(716));
    let mut handle = setup_journal.begin(JournalCredits::new(256)).unwrap();
    let transaction = handle.id();
    let allocation = filesystem
        .allocate_inode(
            None,
            InodeInitialization::regular_file(0o644, 0, 0),
            &mut handle,
        )
        .unwrap();
    let inode = filesystem.internal_iget(allocation.inode()).unwrap();
    let physical = allocate_contiguous_blocks(&mut filesystem, 2, &mut handle);
    filesystem
        .insert_extent_mapping(
            &inode,
            LogicalBlock::new(1),
            physical,
            BlockCount::new(1),
            ExtentMappingState::Initialized,
            &mut handle,
        )
        .unwrap();
    filesystem
        .insert_extent_mapping(
            &inode,
            LogicalBlock::new(2),
            PhysicalBlock::new(physical.get() + 1),
            BlockCount::new(1),
            ExtentMappingState::Unwritten,
            &mut handle,
        )
        .unwrap();
    drop(handle);
    let commit = setup_journal.force_commit(transaction).unwrap();
    filesystem
        .metadata_io
        .checkpoint_committed(&commit)
        .unwrap();
    setup_journal.finish_checkpoint_for_test(&commit).unwrap();

    install_credit_limited_journal(&mut filesystem, 717, 75, true);
    let input = vec![0x4e; TEST_BLOCK_SIZE * 4];
    let timestamp = crate::Ext4Timestamp::new(5688, 0);
    inode.set_size(input.len() as u64);
    filesystem
        .reserve_delalloc_range(&inode, LogicalBlock::new(0), 4)
        .unwrap();

    filesystem
        .writeback_ordered_at_with_prealloc_budget(
            &inode,
            0,
            &input,
            input.len() as u64,
            timestamp,
            Ext4SyncIntent::FullMetadata,
            0,
        )
        .expect("stream hole, mapped, unwritten, and trailing-hole mappings");

    assert!(filesystem.pending_checkpoint_count() >= 1);
    assert_eq!(inode.disk_size(), input.len() as u64);
    assert_eq!(inode.ctime(), timestamp);
    assert_eq!(inode.mtime(), timestamp);
    assert!(!inode.has_delalloc_reservations());
    assert_eq!(filesystem.delalloc_reserved_blocks(), 0);
    for logical in 0..4u64 {
        assert!(matches!(
            filesystem.map_blocks(&inode, LogicalBlock::new(logical)),
            Ok(crate::BlockMapping::Mapped { .. })
        ));
    }
    let mut output = vec![0; input.len()];
    assert_eq!(filesystem.read_at(&inode, 0, &mut output), Ok(input.len()));
    assert_eq!(output, input);
}

#[def_test]
fn mapped_full_metadata_writeback_fits_the_eight_credit_boundary() {
    let (mut filesystem, _device) =
        journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let setup_journal = JournalTransactions::new(TransactionId::new(718));
    let mut handle = setup_journal.begin(JournalCredits::new(16)).unwrap();
    let transaction = handle.id();
    let allocation = filesystem
        .allocate_inode(
            None,
            InodeInitialization::regular_file(0o644, 0, 0),
            &mut handle,
        )
        .unwrap();
    let inode = filesystem.internal_iget(allocation.inode()).unwrap();
    let physical = allocate_contiguous_blocks(&mut filesystem, 1, &mut handle);
    filesystem
        .insert_extent_mapping(
            &inode,
            LogicalBlock::new(0),
            physical,
            BlockCount::new(1),
            ExtentMappingState::Initialized,
            &mut handle,
        )
        .unwrap();
    drop(handle);
    let commit = setup_journal.force_commit(transaction).unwrap();
    filesystem
        .metadata_io
        .checkpoint_committed(&commit)
        .unwrap();
    setup_journal.finish_checkpoint_for_test(&commit).unwrap();

    // An 8-credit transaction, exactly the mapped-only
    // FullMetadata bound. Mapped runs add no operation credits and must
    // therefore complete without entering the Restart branch.
    install_credit_limited_journal(&mut filesystem, 719, 8, false);
    let input = vec![0x2a; TEST_BLOCK_SIZE];
    let timestamp = crate::Ext4Timestamp::new(5689, 0);
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
        .expect("mapped FullMetadata writeback fits the minimum journal bound");

    assert_eq!(filesystem.pending_checkpoint_count(), 0);
    assert_eq!(inode.disk_size(), input.len() as u64);
    assert_eq!(inode.ctime(), timestamp);
    assert_eq!(inode.mtime(), timestamp);
    let mut output = vec![0; input.len()];
    assert_eq!(filesystem.read_at(&inode, 0, &mut output), Ok(input.len()));
    assert_eq!(output, input);
}

#[def_test]
fn failed_split_writeback_publishes_only_the_durable_prefix() {
    let (mut filesystem, device) = journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = allocate_checkpointed_regular_inode(&mut filesystem);
    fragment_ordered_writeback_free_space(&mut filesystem);
    install_credit_limited_journal(&mut filesystem, 715, 79, true);
    drop(filesystem.metadata_journal().unwrap());
    let input = vec![0x3d; TEST_BLOCK_SIZE * 6];
    let visible_size = input.len() as u64;
    let timestamp = crate::Ext4Timestamp::new(5685, 0);

    inode.set_size(visible_size);
    filesystem
        .reserve_delalloc_range(&inode, LogicalBlock::new(0), 6)
        .unwrap();
    // The first data barrier and three journal barriers must succeed;
    // fail the data barrier of the restarted transaction.
    device.fail_flush_at(device.flush_count() + 5);
    let failure = filesystem
        .writeback_ordered_at_with_prealloc_budget(
            &inode,
            0,
            &input,
            visible_size,
            timestamp,
            Ext4SyncIntent::FullMetadata,
            0,
        )
        .expect_err("the second transaction data barrier must fail");
    assert_eq!(failure.error(), Ext4Error::Device(DriverError::Io));
    assert_eq!(failure.completed_bytes(), TEST_BLOCK_SIZE * 5);

    // The first five-block transaction and its journal commit are durable.
    // The second transaction's data barrier fails, so only that prefix may
    // advance i_disksize and FullMetadata must remain unpublished.
    assert_eq!(inode.disk_size(), (TEST_BLOCK_SIZE * 5) as u64);
    assert!(inode.has_delalloc_reservations());
    assert_eq!(filesystem.delalloc_reserved_blocks(), 1);
    assert_ne!(inode.ctime(), timestamp);
    assert_ne!(inode.mtime(), timestamp);
    assert!(
        filesystem
            .journal
            .as_ref()
            .expect("installed journal")
            .is_aborted()
    );
}

#[def_test]
fn ordered_writeback_credits_follow_fragmented_unwritten_runs() {
    let (mut filesystem, device) = journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let setup_journal = JournalTransactions::new(TransactionId::new(716));
    let mut handle = setup_journal.begin(JournalCredits::new(1024)).unwrap();
    let transaction = handle.id();
    let allocation = filesystem
        .allocate_inode(
            None,
            InodeInitialization::regular_file(0o644, 0, 0),
            &mut handle,
        )
        .unwrap();
    let inode = filesystem.internal_iget(allocation.inode()).unwrap();
    let physical = allocate_contiguous_blocks(&mut filesystem, 8, &mut handle);
    for logical in 0..8u64 {
        let state = if logical % 2 == 0 {
            ExtentMappingState::Initialized
        } else {
            ExtentMappingState::Unwritten
        };
        filesystem
            .insert_extent_mapping(
                &inode,
                LogicalBlock::new(logical),
                PhysicalBlock::new(physical.get() + logical),
                BlockCount::new(1),
                state,
                &mut handle,
            )
            .unwrap();
    }
    drop(handle);
    let commit = setup_journal.force_commit(transaction).unwrap();
    filesystem
        .metadata_io
        .checkpoint_committed(&commit)
        .unwrap();
    setup_journal.finish_checkpoint_for_test(&commit).unwrap();

    // Revoke-capable journal keeps this transaction running until sync.
    install_credit_limited_journal(&mut filesystem, 717, 341, true);
    let input = vec![0x5c; TEST_BLOCK_SIZE * 8];
    let flushes_before = device.flush_count();
    inode.set_size(input.len() as u64);
    filesystem
        .writeback_ordered_at_with_prealloc_budget(
            &inode,
            0,
            &input,
            input.len() as u64,
            crate::Ext4Timestamp::new(5686, 0),
            Ext4SyncIntent::DataOnly,
            0,
        )
        .expect("charge the four unwritten mappings instead of eight data blocks");

    // Four unwritten conversions fit in one transaction. Charging every
    // logical block as a hole would exceed this journal and split/error.
    assert_eq!(device.flush_count(), flushes_before + 1);
    assert_eq!(
        filesystem.map_blocks(&inode, LogicalBlock::new(0)),
        Ok(crate::BlockMapping::Mapped {
            physical,
            len: BlockCount::new(8),
            flags: crate::BlockMappingFlags::empty(),
        })
    );
    let mut output = vec![0; input.len()];
    assert_eq!(filesystem.read_at(&inode, 0, &mut output), Ok(input.len()));
    assert_eq!(output, input);
}

#[def_test]
fn ordered_writeback_reuses_preallocated_unwritten_tail() {
    let (mut filesystem, _device) =
        journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = allocate_checkpointed_regular_inode(&mut filesystem);
    install_test_internal_journal(&mut filesystem, 714);
    let first = vec![0x41; TEST_BLOCK_SIZE * 4];
    let second = vec![0x62; TEST_BLOCK_SIZE * 4];
    let free_before_write = filesystem.free_blocks_count();

    inode.set_size(first.len() as u64);
    filesystem
        .writeback_ordered_at(
            &inode,
            0,
            &first,
            first.len() as u64,
            crate::Ext4Timestamp::new(5681, 0),
            Ext4SyncIntent::DataOnly,
        )
        .unwrap();
    install_test_internal_journal(&mut filesystem, 715);
    inode.set_size((first.len() + second.len()) as u64);
    filesystem
        .writeback_ordered_at(
            &inode,
            first.len() as u64,
            &second,
            (first.len() + second.len()) as u64,
            crate::Ext4Timestamp::new(5682, 0),
            Ext4SyncIntent::DataOnly,
        )
        .unwrap();

    assert_eq!(filesystem.free_blocks_count(), free_before_write - 8);
    assert_eq!(
        filesystem.map_blocks(&inode, LogicalBlock::new(0)),
        Ok(crate::BlockMapping::Mapped {
            physical: PhysicalBlock::new(6),
            len: BlockCount::new(8),
            flags: crate::BlockMappingFlags::empty(),
        })
    );
    let mut output = vec![0; TEST_BLOCK_SIZE * 8];
    assert_eq!(
        filesystem.read_at(&inode, 0, &mut output).unwrap(),
        output.len()
    );
    assert_eq!(&output[..first.len()], &first);
    assert_eq!(&output[first.len()..], &second);
}

#[def_test]
fn ordered_writeback_converts_unwritten_extent_and_zero_fills_partial_block() {
    let (mut filesystem, _device) =
        journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let journal = JournalTransactions::new(TransactionId::new(721));
    let mut handle = journal.begin(JournalCredits::new(256)).unwrap();
    let transaction = handle.id();
    let physical = allocate_contiguous_blocks(&mut filesystem, 1, &mut handle);
    let allocation = filesystem
        .allocate_inode(
            None,
            InodeInitialization::regular_file(0o644, 0, 0),
            &mut handle,
        )
        .unwrap();
    let inode = filesystem.internal_iget(allocation.inode()).unwrap();
    filesystem
        .insert_extent_mapping(
            &inode,
            LogicalBlock::new(0),
            physical,
            BlockCount::new(1),
            ExtentMappingState::Unwritten,
            &mut handle,
        )
        .unwrap();
    drop(handle);
    let commit = journal.force_commit(transaction).unwrap();
    filesystem
        .metadata_io
        .checkpoint_committed(&commit)
        .unwrap();
    journal.finish_checkpoint_for_test(&commit).unwrap();
    install_test_internal_journal(&mut filesystem, 722);

    inode.set_size(4);
    filesystem
        .writeback_ordered_at(
            &inode,
            2,
            b"xy",
            4,
            crate::Ext4Timestamp::new(9, 0),
            Ext4SyncIntent::DataOnly,
        )
        .unwrap();

    assert_eq!(
        filesystem.map_blocks(&inode, LogicalBlock::new(0)),
        Ok(crate::BlockMapping::Mapped {
            physical,
            len: BlockCount::new(1),
            flags: crate::BlockMappingFlags::empty(),
        })
    );
    let mut output = vec![0xff; 4];
    assert_eq!(filesystem.read_at(&inode, 0, &mut output).unwrap(), 4);
    assert_eq!(&output, &[0, 0, b'x', b'y']);
}

#[def_test]
fn regular_file_mutation_accepts_huge_file_feature_for_sector_accounted_inode() {
    let (mut filesystem, _device) =
        journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = allocate_checkpointed_regular_inode(&mut filesystem);
    install_test_internal_journal(&mut filesystem, 768);
    set_allocator_feature_bits(
        &mut filesystem,
        features::CompatFeatures::empty(),
        features::ReadOnlyCompatFeatures::HUGE_FILE,
    );

    inode.set_size(1);
    filesystem
        .writeback_ordered_at(
            &inode,
            0,
            b"x",
            1,
            crate::Ext4Timestamp::new(35, 0),
            Ext4SyncIntent::FullMetadata,
        )
        .expect("write ordinary inode on huge_file filesystem");
    filesystem
        .truncate_regular_inode(&inode, 0, crate::Ext4Timestamp::new(36, 0))
        .expect("truncate ordinary inode on huge_file filesystem");
}

#[def_test]
fn regular_file_mutation_rejects_inode_using_huge_file_accounting() {
    let (mut filesystem, _device) =
        journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = allocate_checkpointed_regular_inode(&mut filesystem);
    install_test_internal_journal(&mut filesystem, 769);
    set_allocator_feature_bits(
        &mut filesystem,
        features::CompatFeatures::empty(),
        features::ReadOnlyCompatFeatures::HUGE_FILE,
    );

    let journal = JournalTransactions::new(TransactionId::new(770));
    let mut handle = journal.begin(JournalCredits::new(1)).unwrap();
    let transaction = handle.id();
    filesystem
        .update_inode_flags_timestamps_metadata(
            &inode,
            inode.flags() | crate::disk::inode::EXT4_HUGE_FILE_FL,
            crate::Ext4Timestamp::new(37, 0),
            &mut handle,
        )
        .expect("set huge-file inode flag");
    drop(handle);
    let commit = journal.force_commit(transaction).unwrap();
    filesystem
        .metadata_io
        .checkpoint_committed(&commit)
        .unwrap();
    journal.finish_checkpoint_for_test(&commit).unwrap();

    let failure = filesystem
        .writeback_ordered_at(
            &inode,
            0,
            b"x",
            1,
            crate::Ext4Timestamp::new(38, 0),
            Ext4SyncIntent::FullMetadata,
        )
        .expect_err("huge-file block accounting remains unsupported");
    assert_eq!(failure.completed_bytes(), 0);
    assert_eq!(
        failure.error(),
        Ext4Error::Unsupported(UnsupportedKind::HugeFile)
    );
}

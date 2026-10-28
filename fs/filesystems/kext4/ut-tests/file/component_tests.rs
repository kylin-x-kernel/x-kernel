// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Component invariants exercised in the kernel with synthetic metadata.

use alloc::{sync::Arc, vec};
use core::cell::Cell;

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
        install_test_internal_journal, journal_allocator_test_filesystem, prealloc_count,
        prealloc_twin_count, set_allocator_feature_bits,
    },
};

#[def_test]
fn unwritten_conversion_no_space_before_metadata_does_not_abort_journal() {
    let (mut filesystem, _device) =
        journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
    let setup = JournalTransactions::new(TransactionId::new(722));
    let mut handle = setup.begin(JournalCredits::new(1024)).unwrap();
    let physical = allocate_contiguous_blocks(&mut filesystem, 6, &mut handle);
    for (logical, start, count) in [(0, 0, 3), (4, 3, 1), (6, 4, 1), (8, 5, 1)] {
        filesystem
            .insert_extent_mapping(
                &inode,
                LogicalBlock::new(logical),
                PhysicalBlock::new(physical.get() + start),
                BlockCount::new(count),
                ExtentMappingState::Unwritten,
                &mut handle,
            )
            .unwrap();
    }
    let mut spare = None;
    while filesystem.free_blocks_count() != 0 {
        spare = Some(
            filesystem
                .allocate_block(None, &mut handle)
                .unwrap()
                .block(),
        );
    }
    inode.set_size((TEST_BLOCK_SIZE * 9) as u64);
    filesystem
        .update_regular_inode_write_metadata(
            &inode,
            (TEST_BLOCK_SIZE * 9) as u64,
            RegularWriteMetadata::SizeOnly,
            &mut handle,
        )
        .unwrap();
    drop(handle);
    let commit = setup.force_commit(TransactionId::new(722)).unwrap();
    filesystem
        .metadata_io
        .checkpoint_committed(&commit)
        .unwrap();
    setup.finish_checkpoint_for_test(&commit).unwrap();
    install_credit_limited_journal(&mut filesystem, 723, 341, true);
    let journal = filesystem.metadata_journal().unwrap();
    let input = vec![0x93; TEST_BLOCK_SIZE];

    // Converting the middle block creates six inline entries and needs a new
    // tree block. Data I/O succeeds, but allocation fails before metadata access.
    let failure = filesystem
        .writeback_ordered_at_with_prealloc_budget(
            &inode,
            TEST_BLOCK_SIZE as u64,
            &input,
            inode.size(),
            Ext4SyncIntent::DataOnly,
            0,
            || true,
        )
        .unwrap_err();
    assert_eq!(failure.error(), Ext4Error::NoSpace);
    assert_eq!(failure.completed_bytes(), 0);
    assert!(!journal.is_aborted());
    assert!(matches!(
        filesystem.map_blocks(&inode, LogicalBlock::new(1)).unwrap(),
        BlockMapping::Unwritten { .. }
    ));

    // Free one unrelated block, then retry on the same mounted journal.
    let mut handle = journal.begin(JournalCredits::new(8)).unwrap();
    filesystem
        .release_allocated_block(spare.unwrap(), &mut handle)
        .unwrap();
    filesystem
        .complete_metadata_mutation(handle, Ok(()))
        .unwrap();
    filesystem
        .writeback_ordered_at_with_prealloc_budget(
            &inode,
            TEST_BLOCK_SIZE as u64,
            &input,
            inode.size(),
            Ext4SyncIntent::DataOnly,
            0,
            || true,
        )
        .unwrap();
    filesystem
        .sync_inode(&inode, Ext4SyncIntent::DataOnly)
        .unwrap();
    let mut output = vec![0; TEST_BLOCK_SIZE];
    assert_eq!(
        filesystem.read_at(&inode, TEST_BLOCK_SIZE as u64, &mut output),
        Ok(TEST_BLOCK_SIZE)
    );
    assert_eq!(output, input);
    assert!(!journal.is_aborted());
}

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
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
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
            Ext4SyncIntent::DataOnly,
            || true,
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
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
    install_test_internal_journal(&mut filesystem, 711);

    inode.set_size(3);
    filesystem
        .writeback_ordered_at(&inode, 0, b"abc", 3, Ext4SyncIntent::DataOnly, || true)
        .unwrap();
    inode.set_size(6);
    filesystem
        .writeback_ordered_at(&inode, 3, b"def", 6, Ext4SyncIntent::DataOnly, || true)
        .unwrap();

    let mut output = vec![0; 6];
    assert_eq!(filesystem.read_at(&inode, 0, &mut output).unwrap(), 6);
    assert_eq!(&output, b"abcdef");
    assert_eq!(inode.size(), 6);
}

#[def_test]
fn ordered_writeback_registers_prealloc_space_without_disk_tail() {
    let (mut filesystem, _device) =
        journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
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
            Ext4SyncIntent::DataOnly,
            || true,
        )
        .unwrap();

    assert_eq!(inode.size(), input.len() as u64);
    // Only the dirty window reached the bitmap and the extent tree.
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
    // The normalized surplus is an in-memory space, not an on-disk tail.
    assert_eq!(prealloc_count(&inode), 1);
    assert_eq!(prealloc_twin_count(&filesystem), 1);

    // Legacy on-disk-tail cleanup has nothing to do and leaves the space alone.
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
    assert_eq!(prealloc_count(&inode), 1);
}

#[def_test]
fn ordered_writeback_prealloc_budget_zero_allocates_only_dirty_blocks() {
    let (mut filesystem, _device) =
        journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
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
            Ext4SyncIntent::DataOnly,
            0,
            || true,
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
    assert_eq!(prealloc_count(&inode), 0);
}

#[def_test]
fn ordered_writeback_restarts_transaction_for_actual_allocation_runs() {
    let (mut filesystem, device) = journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
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
            Ext4SyncIntent::DataOnly,
            0,
            || true,
        )
        .expect("restart after the actual allocation runs exhaust one transaction");

    // The test leaves only single-block free runs and limits a transaction
    // to 79 credits. The fifth insertion grows the inline extent root;
    // extending for the sixth run then commits that prefix and restarts.
    assert_eq!(filesystem.pending_checkpoint_count(), 1);
    // The restarted prefix has data, recovery, activation and commit barriers.
    // Tail data stays attached to the new running transaction until sync.
    assert_eq!(device.flush_count(), flushes_before + 4);
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
    let inode = Arc::new(filesystem.internal_iget(allocation.inode()).unwrap());
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
    // tail registers the optional preallocation budget as an in-memory space.
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
            Ext4SyncIntent::DataOnly,
            4,
            || true,
        )
        .expect("five transaction restarts preserve the one-shot preallocation budget");

    assert_eq!(filesystem.pending_checkpoint_count(), 5);
    assert_eq!(inode.disk_size(), input.len() as u64);
    assert!(!inode.has_delalloc_reservations());
    assert_eq!(filesystem.delalloc_reserved_blocks(), 0);
    // i_blocks includes the extent leaf already allocated by setup.
    assert_eq!(
        inode.blocks(),
        blocks_before_write + u64::try_from(9 * (TEST_BLOCK_SIZE / 512)).unwrap()
    );
    assert_eq!(filesystem.free_blocks_count(), free_before_write - 9);
    let Ok(crate::BlockMapping::Mapped {
        len: dirty_tail_len,
        ..
    }) = filesystem.map_blocks(&inode, LogicalBlock::new(10))
    else {
        panic!("dirty tail must be mapped after writeback");
    };
    assert_eq!(dirty_tail_len, BlockCount::new(4));
    // The surplus past the dirty tail never reached the extent tree.
    assert_eq!(
        filesystem.map_blocks(&inode, LogicalBlock::new(14)),
        Ok(crate::BlockMapping::Hole {
            len: BlockCount::new(u32::MAX),
            flags: crate::BlockMappingFlags::empty(),
        })
    );
    assert_eq!(prealloc_count(&inode), 1);
    assert_eq!(prealloc_twin_count(&filesystem), 1);
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
    let inode = Arc::new(filesystem.internal_iget(allocation.inode()).unwrap());
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
    let original_times = (inode.ctime(), inode.mtime());
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
            Ext4SyncIntent::FullMetadata,
            0,
            || true,
        )
        .expect("stream hole, mapped, unwritten, and trailing-hole mappings");

    assert!(filesystem.pending_checkpoint_count() >= 1);
    assert_eq!(inode.disk_size(), input.len() as u64);
    assert_eq!((inode.ctime(), inode.mtime()), original_times);
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
    let inode = Arc::new(filesystem.internal_iget(allocation.inode()).unwrap());
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
    let original_times = (inode.ctime(), inode.mtime());
    inode.set_size(input.len() as u64);

    filesystem
        .writeback_ordered_at(
            &inode,
            0,
            &input,
            input.len() as u64,
            Ext4SyncIntent::FullMetadata,
            || true,
        )
        .expect("mapped FullMetadata writeback fits the minimum journal bound");

    assert_eq!(filesystem.pending_checkpoint_count(), 0);
    assert_eq!(inode.disk_size(), input.len() as u64);
    assert_eq!((inode.ctime(), inode.mtime()), original_times);
    let mut output = vec![0; input.len()];
    assert_eq!(filesystem.read_at(&inode, 0, &mut output), Ok(input.len()));
    assert_eq!(output, input);
}

#[def_test]
fn failed_split_writeback_publishes_only_the_durable_prefix() {
    let (mut filesystem, device) = journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
    fragment_ordered_writeback_free_space(&mut filesystem);
    install_credit_limited_journal(&mut filesystem, 715, 79, true);
    drop(filesystem.metadata_journal().unwrap());
    let input = vec![0x3d; TEST_BLOCK_SIZE * 11];
    let visible_size = input.len() as u64;
    let timestamp = crate::Ext4Timestamp::new(5685, 0);

    inode.set_size(visible_size);
    filesystem
        .reserve_delalloc_range(&inode, LogicalBlock::new(0), 11)
        .unwrap();
    // The first data barrier and three journal barriers must succeed;
    // fail the capacity-restart data barrier of the second transaction.
    device.fail_flush_at(device.flush_count() + 5);
    let failure = filesystem
        .writeback_ordered_at_with_prealloc_budget(
            &inode,
            0,
            &input,
            visible_size,
            Ext4SyncIntent::FullMetadata,
            0,
            || true,
        )
        .expect_err("the second transaction data barrier must fail");
    assert_eq!(failure.error(), Ext4Error::Device(DriverError::Io));
    assert_eq!(failure.completed_bytes(), TEST_BLOCK_SIZE * 5);

    // The first five-block transaction and its journal commit are durable.
    // The second transaction's data barrier fails, so only that prefix may
    // advance i_disksize and FullMetadata must remain unpublished.
    assert_eq!(inode.disk_size(), (TEST_BLOCK_SIZE * 5) as u64);
    assert!(inode.has_delalloc_reservations());
    assert_eq!(filesystem.delalloc_reserved_blocks(), 6);
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
    let inode = Arc::new(filesystem.internal_iget(allocation.inode()).unwrap());
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
            Ext4SyncIntent::DataOnly,
            0,
            || true,
        )
        .expect("charge the four unwritten mappings instead of eight data blocks");

    // Four unwritten conversions fit in one transaction. Charging every
    // logical block as a hole would exceed this journal and split/error.
    assert_eq!(device.flush_count(), flushes_before);
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
fn ordered_writeback_reuses_preallocated_space_tail() {
    let (mut filesystem, _device) =
        journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
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
            Ext4SyncIntent::DataOnly,
            || true,
        )
        .unwrap();
    // Only the dirty window is marked; the sampled surplus stays in memory and
    // the following blocks remain a hole in the extent tree.
    assert_eq!(filesystem.free_blocks_count(), free_before_write - 4);
    assert_eq!(prealloc_count(&inode), 1);
    assert_eq!(prealloc_twin_count(&filesystem), 1);
    assert!(matches!(
        filesystem.map_blocks(&inode, LogicalBlock::new(4)),
        Ok(crate::BlockMapping::Hole { .. })
    ));

    install_test_internal_journal(&mut filesystem, 715);
    inode.set_size((first.len() + second.len()) as u64);
    filesystem
        .writeback_ordered_at(
            &inode,
            first.len() as u64,
            &second,
            (first.len() + second.len()) as u64,
            Ext4SyncIntent::DataOnly,
            || true,
        )
        .unwrap();

    // The second run consumed the whole space, so it marked those blocks itself
    // instead of reusing an on-disk unwritten tail.
    assert_eq!(prealloc_count(&inode), 0);
    assert_eq!(prealloc_twin_count(&filesystem), 0);
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
fn writeback_consumes_space_prefix_and_keeps_tail() {
    let (mut filesystem, _device) =
        journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
    install_test_internal_journal(&mut filesystem, 719);
    let first = vec![0x21; TEST_BLOCK_SIZE * 4];
    let second = vec![0x43; TEST_BLOCK_SIZE * 2];
    let free_before_write = filesystem.free_blocks_count();

    inode.set_size(first.len() as u64);
    filesystem
        .writeback_ordered_at(
            &inode,
            0,
            &first,
            first.len() as u64,
            Ext4SyncIntent::DataOnly,
            || true,
        )
        .unwrap();

    install_test_internal_journal(&mut filesystem, 720);
    inode.set_size((first.len() + second.len()) as u64);
    filesystem
        .writeback_ordered_at(
            &inode,
            first.len() as u64,
            &second,
            (first.len() + second.len()) as u64,
            Ext4SyncIntent::DataOnly,
            || true,
        )
        .unwrap();

    // The second run took the space's prefix; its unconsumed tail stays a
    // registered space rather than being re-marked or lost.
    assert_eq!(filesystem.free_blocks_count(), free_before_write - 6);
    assert_eq!(prealloc_count(&inode), 1);
    assert_eq!(prealloc_twin_count(&filesystem), 1);
    assert_eq!(
        filesystem.map_blocks(&inode, LogicalBlock::new(0)),
        Ok(crate::BlockMapping::Mapped {
            physical: PhysicalBlock::new(6),
            len: BlockCount::new(6),
            flags: crate::BlockMappingFlags::empty(),
        })
    );
    assert_eq!(
        filesystem.map_blocks(&inode, LogicalBlock::new(6)),
        Ok(crate::BlockMapping::Hole {
            len: BlockCount::new(u32::MAX),
            flags: crate::BlockMappingFlags::empty(),
        })
    );
}

#[def_test]
fn writeback_settlement_drops_spaces_without_open_writer() {
    let (mut filesystem, _device) =
        journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
    install_test_internal_journal(&mut filesystem, 719);
    let input = vec![0x55; TEST_BLOCK_SIZE * 4];
    let free_before_write = filesystem.free_blocks_count();

    inode.set_size(input.len() as u64);
    // No delalloc reservation and no writable open: settlement drops the
    // space the writeback just produced (Linux ext4_da_update_reserve_space).
    filesystem
        .writeback_ordered_at_with_prealloc_budget(
            &inode,
            0,
            &input,
            input.len() as u64,
            Ext4SyncIntent::DataOnly,
            4,
            || false,
        )
        .unwrap();

    assert_eq!(filesystem.free_blocks_count(), free_before_write - 4);
    assert_eq!(prealloc_count(&inode), 0);
    assert_eq!(prealloc_twin_count(&filesystem), 0);
    assert_eq!(
        filesystem.map_blocks(&inode, LogicalBlock::new(4)),
        Ok(crate::BlockMapping::Hole {
            len: BlockCount::new(u32::MAX),
            flags: crate::BlockMappingFlags::empty(),
        })
    );
}

#[def_test]
fn writeback_admission_excludes_the_prefix_a_registered_space_serves() {
    let (mut filesystem, _device) =
        journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
    inode.disable_extra_isize_expansion();
    install_test_internal_journal(&mut filesystem, 727);
    let input = vec![0x61; TEST_BLOCK_SIZE * 4];
    inode.set_size(input.len() as u64);
    filesystem
        .writeback_ordered_at_with_prealloc_budget(
            &inode,
            0,
            &input,
            input.len() as u64,
            Ext4SyncIntent::DataOnly,
            4,
            || true,
        )
        .unwrap();
    // The birth sampled logical 4..8 as this inode's space.
    assert_eq!(prealloc_count(&inode), 1);

    // Admission for the next hole run must not count blocks the space already
    // covers as new physical demand: consuming them allocates nothing, and the
    // extent insert still fits the existing leaf.
    assert_eq!(
        filesystem
            .ordered_writeback_allocation_blocks(&inode, LogicalBlock::new(4), 4)
            .unwrap(),
        0
    );
    // A run outside the space still admits all of its data blocks.
    assert_eq!(
        filesystem
            .ordered_writeback_allocation_blocks(&inode, LogicalBlock::new(8), 4)
            .unwrap(),
        4
    );
}

#[def_test]
fn writeback_settlement_re_reads_the_writer_gate_at_each_settlement() {
    let (mut filesystem, _device) =
        journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
    install_test_internal_journal(&mut filesystem, 723);
    // Birth one space and keep it: this settlement sees a writable open.
    let first = vec![0x31; TEST_BLOCK_SIZE * 4];
    inode.set_size(first.len() as u64);
    filesystem
        .writeback_ordered_at_with_prealloc_budget(
            &inode,
            0,
            &first,
            first.len() as u64,
            Ext4SyncIntent::DataOnly,
            4,
            || true,
        )
        .unwrap();
    assert_eq!(prealloc_count(&inode), 1);

    // Fragment the tail with mapped runs so the next call commits one prefix
    // per hole and settles more than once.
    let setup_journal = JournalTransactions::new(TransactionId::new(724));
    let mut handle = setup_journal.begin(JournalCredits::new(1024)).unwrap();
    let transaction = handle.id();
    for logical in [8u64, 10, 12, 14, 16] {
        let physical = filesystem
            .allocate_block(None, &mut handle)
            .unwrap()
            .block();
        filesystem
            .insert_extent_mapping(
                &inode,
                LogicalBlock::new(logical),
                physical,
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

    install_credit_limited_journal(&mut filesystem, 725, 75, true);
    // The first settlement reports a writable open; every later one reports
    // closed.
    let calls = Cell::new(0u32);
    let gate = || {
        let seen = calls.get();
        calls.set(seen + 1);
        seen == 0
    };
    let start = TEST_BLOCK_SIZE * 8;
    let second = vec![0x52; TEST_BLOCK_SIZE * 14];
    inode.set_size(u64::try_from(start + second.len()).unwrap());
    filesystem
        .writeback_ordered_at_with_prealloc_budget(
            &inode,
            u64::try_from(start).unwrap(),
            &second,
            inode.size(),
            Ext4SyncIntent::DataOnly,
            4,
            gate,
        )
        .unwrap();

    // A snapshot taken once at entry would keep every space; the live gate must
    // drop them at the first settlement that reports closed.
    assert!(calls.get() >= 2, "the scenario must settle more than once");
    assert_eq!(prealloc_count(&inode), 0);
    assert_eq!(prealloc_twin_count(&filesystem), 0);
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
    let inode = Arc::new(filesystem.internal_iget(allocation.inode()).unwrap());
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
        .writeback_ordered_at(&inode, 2, b"xy", 4, Ext4SyncIntent::DataOnly, || true)
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
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
    install_test_internal_journal(&mut filesystem, 768);
    set_allocator_feature_bits(
        &mut filesystem,
        features::CompatFeatures::empty(),
        features::ReadOnlyCompatFeatures::HUGE_FILE,
    );

    inode.set_size(1);
    filesystem
        .writeback_ordered_at(&inode, 0, b"x", 1, Ext4SyncIntent::FullMetadata, || true)
        .expect("write ordinary inode on huge_file filesystem");
    filesystem
        .truncate_regular_inode(&inode, 0, crate::Ext4Timestamp::new(36, 0))
        .expect("truncate ordinary inode on huge_file filesystem");
}

#[def_test]
fn regular_file_mutation_rejects_inode_using_huge_file_accounting() {
    let (mut filesystem, _device) =
        journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
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
        .writeback_ordered_at(&inode, 0, b"x", 1, Ext4SyncIntent::FullMetadata, || true)
        .expect_err("huge-file block accounting remains unsupported");
    assert_eq!(failure.completed_bytes(), 0);
    assert_eq!(
        failure.error(),
        Ext4Error::Unsupported(UnsupportedKind::HugeFile)
    );
}

#[def_test]
fn mapped_full_metadata_writeback_keeps_pending_frees_on_a_full_filesystem() {
    writeback_without_growth_preserves_pending_frees(ExtentMappingState::Initialized, 1, 0, 0);
}

#[def_test]
fn unwritten_full_conversion_keeps_pending_frees_on_a_full_filesystem() {
    writeback_without_growth_preserves_pending_frees(ExtentMappingState::Unwritten, 1, 0, 0);
}

#[def_test]
fn unwritten_partial_conversion_uses_spare_inline_entries_without_commit() {
    writeback_without_growth_preserves_pending_frees(ExtentMappingState::Unwritten, 3, 1, 0);
}

#[def_test]
fn small_hole_uses_leaf_capacity_without_committing_frees() {
    writeback_without_growth_preserves_pending_frees(ExtentMappingState::Initialized, 1, 1, 1);
}

fn writeback_without_growth_preserves_pending_frees(
    state: ExtentMappingState,
    initial_blocks: u32,
    write_block: u64,
    reusable_blocks: u64,
) {
    let (mut filesystem, device) = journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
    inode.disable_extra_isize_expansion();
    let setup = JournalTransactions::new(TransactionId::new(760));
    let mut handle = setup.begin(JournalCredits::new(1024)).unwrap();
    let physical = allocate_contiguous_blocks(&mut filesystem, initial_blocks, &mut handle);
    filesystem
        .insert_extent_mapping(
            &inode,
            LogicalBlock::new(0),
            physical,
            BlockCount::new(initial_blocks),
            state,
            &mut handle,
        )
        .unwrap();
    let mut spare = None;
    while filesystem.free_blocks_count() > reusable_blocks {
        spare = Some(
            filesystem
                .allocate_block(None, &mut handle)
                .unwrap()
                .block(),
        );
    }
    inode.set_size((u64::from(initial_blocks).max(write_block + 1)) * TEST_BLOCK_SIZE as u64);
    filesystem
        .update_regular_inode_write_metadata(
            &inode,
            inode.size(),
            RegularWriteMetadata::SizeOnly,
            &mut handle,
        )
        .unwrap();
    handle.stop().unwrap();
    let commit = setup.force_commit(TransactionId::new(760)).unwrap();
    filesystem
        .metadata_io
        .checkpoint_committed(&commit)
        .unwrap();
    setup.finish_checkpoint_for_test(&commit).unwrap();
    install_credit_limited_journal(&mut filesystem, 761, 341, true);
    let journal = filesystem.metadata_journal().unwrap();
    let mut handle = journal.begin(JournalCredits::new(3)).unwrap();
    filesystem
        .release_allocated_block(spare.unwrap(), &mut handle)
        .unwrap();
    let transaction = handle.id();
    handle.stop().unwrap();
    let flushes = device.flush_count();
    let input = vec![0x73; TEST_BLOCK_SIZE];
    let offset = write_block * TEST_BLOCK_SIZE as u64;
    assert_eq!(
        filesystem
            .writeback_ordered_at_with_prealloc_budget(
                &inode,
                offset,
                &input,
                inode.size(),
                Ext4SyncIntent::FullMetadata,
                32,
                || true,
            )
            .unwrap(),
        input.len()
    );
    assert_eq!(device.flush_count(), flushes);
    assert_eq!(
        journal.begin(JournalCredits::new(0)).unwrap().id(),
        transaction
    );
    assert_eq!(filesystem.pending_checkpoint_count(), 0);
    assert!(filesystem.has_pending_block_frees().unwrap());
    filesystem
        .sync_inode(&inode, Ext4SyncIntent::FullMetadata)
        .unwrap();
    assert!(!filesystem.has_pending_block_frees().unwrap());
    let mut output = vec![0; TEST_BLOCK_SIZE];
    assert_eq!(
        filesystem.read_at(&inode, offset, &mut output).unwrap(),
        input.len()
    );
    assert_eq!(output, input);
}

#[def_test]
fn readmission_credit_commit_confirms_prefix_before_later_io_failure() {
    readmission_failure_preserves_prefix(78, false, false, 5);
}

#[def_test]
fn readmission_space_commit_confirms_prefix_before_later_io_failure() {
    readmission_failure_preserves_prefix(1000, true, false, 5);
}

#[def_test]
fn readmission_without_commit_does_not_confirm_prefix() {
    readmission_failure_preserves_prefix(1000, false, false, 0);
}

#[def_test]
fn failed_readmission_commit_does_not_confirm_prefix() {
    readmission_failure_preserves_prefix(78, false, true, 0);
}

fn readmission_failure_preserves_prefix(
    credits: u32,
    fill_space: bool,
    fail_commit: bool,
    completed_blocks: usize,
) {
    let (mut filesystem, device) = journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
    inode.disable_extra_isize_expansion();
    let setup = JournalTransactions::new(TransactionId::new(770));
    let mut handle = setup.begin(JournalCredits::new(1024)).unwrap();
    let physical = filesystem
        .allocate_block(Some(crate::FilesystemBlock::new(6)), &mut handle)
        .unwrap()
        .block();
    assert_eq!(physical, PhysicalBlock::new(6));
    filesystem
        .insert_extent_mapping(
            &inode,
            LogicalBlock::new(4),
            physical,
            BlockCount::new(1),
            ExtentMappingState::Initialized,
            &mut handle,
        )
        .unwrap();
    let spare = filesystem
        .allocate_block(Some(crate::FilesystemBlock::new(7)), &mut handle)
        .unwrap()
        .block();
    assert_eq!(spare, PhysicalBlock::new(7));
    if fill_space {
        while filesystem.free_blocks_count() > 4 {
            filesystem
                .allocate_block(Some(crate::FilesystemBlock::new(12)), &mut handle)
                .unwrap();
        }
    }
    inode.set_size((6 * TEST_BLOCK_SIZE) as u64);
    filesystem
        .update_regular_inode_write_metadata(
            &inode,
            inode.size(),
            RegularWriteMetadata::SizeOnly,
            &mut handle,
        )
        .unwrap();
    handle.stop().unwrap();
    let commit = setup.force_commit(TransactionId::new(770)).unwrap();
    filesystem
        .metadata_io
        .checkpoint_committed(&commit)
        .unwrap();
    setup.finish_checkpoint_for_test(&commit).unwrap();
    install_credit_limited_journal(&mut filesystem, 771, credits, true);
    let journal = filesystem.metadata_journal().unwrap();
    let mut handle = journal.begin(JournalCredits::new(3)).unwrap();
    filesystem
        .release_allocated_block(spare, &mut handle)
        .unwrap();
    handle.stop().unwrap();
    filesystem
        .reserve_delalloc_range(&inode, LogicalBlock::new(0), 4)
        .unwrap();
    filesystem
        .reserve_delalloc_range(&inode, LogicalBlock::new(5), 1)
        .unwrap();
    let flushes = device.flush_count();
    if fail_commit {
        device.fail_flush_at(flushes + 1);
    } else {
        // The initial hole occupies 8..12. Reuse of block 7 proves commit;
        // without a commit the last hole must skip it and allocate block 12.
        device.fail_write_block(if completed_blocks == 0 {
            PhysicalBlock::new(12)
        } else {
            spare
        });
    }
    let failure = filesystem
        .writeback_ordered_at_with_prealloc_budget(
            &inode,
            0,
            &vec![0x6b; 6 * TEST_BLOCK_SIZE],
            inode.size(),
            Ext4SyncIntent::FullMetadata,
            0,
            || true,
        )
        .unwrap_err();
    assert_eq!(failure.error(), Ext4Error::Device(DriverError::Io));
    assert_eq!(
        failure.completed_bytes(),
        completed_blocks * TEST_BLOCK_SIZE
    );
    assert_eq!(
        filesystem.delalloc_reserved_blocks(),
        if completed_blocks == 0 { 5 } else { 1 }
    );
    assert_eq!(
        filesystem.pending_checkpoint_count() > 0,
        completed_blocks != 0
    );
    if !fail_commit && completed_blocks == 0 {
        assert_eq!(device.flush_count(), flushes);
    }
}

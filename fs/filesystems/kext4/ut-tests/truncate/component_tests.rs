// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Component invariants exercised in the kernel with synthetic metadata.

use alloc::{sync::Arc, vec};

use unittest::def_test;

use crate::{
    BlockCount, BlockMapping, BlockMappingFlags, Ext4SyncIntent, FilesystemBlock, LogicalBlock,
    extent::ExtentMappingState,
    file::RegularWriteMetadata,
    jbd2::{JournalCredits, JournalTransactions, TransactionId},
    superblock::test_fixtures::{
        TEST_BLOCK_SIZE, TEST_FREE_BLOCKS, allocate_checkpointed_regular_inode,
        allocate_contiguous_blocks, fallocate_vfs, install_test_internal_journal,
        journal_allocator_test_filesystem,
    },
};

#[def_test]
fn truncate_releases_delalloc_beyond_visible_eof_even_without_disk_shrink() {
    let (mut filesystem, _device) =
        journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = allocate_checkpointed_regular_inode(&mut filesystem);
    install_test_internal_journal(&mut filesystem, 391);
    let block_size = u64::try_from(TEST_BLOCK_SIZE).unwrap();
    let visible_size = block_size * 6;
    let truncated_size = block_size * 3;

    inode.set_size(visible_size);
    filesystem
        .reserve_delalloc_range_into(&inode, LogicalBlock::new(0), 6)
        .unwrap();
    filesystem
        .truncate_regular_inode(&inode, truncated_size, crate::Ext4Timestamp::new(10, 0))
        .unwrap();

    assert_eq!(inode.disk_size(), truncated_size);
    assert_eq!(inode.size(), truncated_size);
    assert_eq!(filesystem.delalloc_reserved_block_count(), 3);
    assert!(matches!(
        filesystem.report_mapping(&inode, LogicalBlock::new(0)),
        Ok(BlockMapping::Hole {
            len,
            flags: BlockMappingFlags::DELAYED,
        }) if len == BlockCount::new(3)
    ));
    assert!(matches!(
        filesystem.report_mapping(&inode, LogicalBlock::new(3)),
        Ok(BlockMapping::Hole { flags, .. }) if flags.is_empty()
    ));

    inode.set_size(visible_size);
    filesystem
        .reserve_delalloc_range_into(&inode, LogicalBlock::new(3), 3)
        .unwrap();
    filesystem
        .truncate_regular_inode(&inode, truncated_size, crate::Ext4Timestamp::new(11, 0))
        .unwrap();

    assert_eq!(inode.disk_size(), truncated_size);
    assert_eq!(inode.size(), truncated_size);
    assert_eq!(filesystem.delalloc_reserved_block_count(), 3);
    assert!(matches!(
        filesystem.report_mapping(&inode, LogicalBlock::new(3)),
        Ok(BlockMapping::Hole { flags, .. }) if flags.is_empty()
    ));
}

#[def_test]
fn prepared_regular_shrink_leaves_visible_size_for_vfs_pagecache() {
    let (mut filesystem, _device) =
        journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = allocate_checkpointed_regular_inode(&mut filesystem);
    install_test_internal_journal(&mut filesystem, 392);
    let block_size = u64::try_from(TEST_BLOCK_SIZE).unwrap();
    let old_size = block_size * 2;
    let new_size = block_size;

    inode.set_size(old_size);
    filesystem
        .commit_regular_inode_write_metadata(
            &inode,
            old_size,
            RegularWriteMetadata::Full {
                timestamp: crate::Ext4Timestamp::new(12, 0),
            },
        )
        .unwrap();
    filesystem
        .prepare_regular_inode_truncate(&inode, new_size, crate::Ext4Timestamp::new(13, 0))
        .unwrap();

    assert_eq!(inode.disk_size(), new_size);
    assert_eq!(inode.size(), old_size);

    inode.set_size(new_size);
    filesystem
        .finish_regular_inode_shrink(&inode, new_size)
        .unwrap();
    assert_eq!(inode.disk_size(), new_size);
    assert_eq!(inode.size(), new_size);
}

#[def_test]
fn truncate_grow_keeps_new_range_sparse_and_zero_reading() {
    let (mut filesystem, _device) =
        journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = allocate_checkpointed_regular_inode(&mut filesystem);
    install_test_internal_journal(&mut filesystem, 731);
    let timestamp = crate::Ext4Timestamp::new(17, 0);
    let new_size = u64::try_from(TEST_BLOCK_SIZE * 2 + 7).unwrap();

    filesystem
        .truncate_regular_inode(&inode, new_size, timestamp)
        .unwrap();

    assert_eq!(inode.size(), new_size);
    assert_eq!(inode.ctime(), timestamp);
    assert_eq!(inode.mtime(), timestamp);
    assert_eq!(
        filesystem.map_blocks(&inode, LogicalBlock::new(0)),
        Ok(crate::BlockMapping::Hole {
            len: BlockCount::new(u32::MAX),
            flags: crate::BlockMappingFlags::empty(),
        })
    );
    let mut output = [0xff; 4];
    assert_eq!(
        filesystem
            .read_at(&inode, new_size - output.len() as u64, &mut output)
            .unwrap(),
        output.len()
    );
    assert_eq!(output, [0; 4]);
}

#[def_test]
fn truncate_grow_zeroes_mapped_old_eof_tail() {
    let (mut filesystem, _device) =
        journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
    install_test_internal_journal(&mut filesystem, 732);
    let input = vec![0x6d; TEST_BLOCK_SIZE];
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
        .unwrap();
    install_test_internal_journal(&mut filesystem, 733);
    let journal = JournalTransactions::new(TransactionId::new(734));
    let mut handle = journal.begin(JournalCredits::new(8)).unwrap();
    let transaction = handle.id();
    filesystem
        .update_regular_inode_size_metadata(&inode, 23, RegularWriteMetadata::SizeOnly, &mut handle)
        .unwrap();
    drop(handle);
    let commit = journal.force_commit(transaction).unwrap();
    filesystem
        .metadata_io
        .checkpoint_committed(&commit)
        .unwrap();
    journal.finish_checkpoint_for_test(&commit).unwrap();
    install_test_internal_journal(&mut filesystem, 735);
    inode.set_size(23);

    filesystem
        .truncate_regular_inode(
            &inode,
            TEST_BLOCK_SIZE as u64,
            crate::Ext4Timestamp::new(19, 0),
        )
        .unwrap();

    let mut output = vec![0xff; TEST_BLOCK_SIZE];
    assert_eq!(
        filesystem.read_at(&inode, 0, &mut output).unwrap(),
        TEST_BLOCK_SIZE
    );
    assert_eq!(&output[..23], &[0x6d; 23]);
    assert!(output[23..].iter().all(|byte| *byte == 0));
}

#[def_test]
fn truncate_shrink_releases_tail_blocks_and_zeroes_partial_eof_block() {
    let (mut filesystem, _device) =
        journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
    install_test_internal_journal(&mut filesystem, 741);
    let input = vec![0xa5; TEST_BLOCK_SIZE * 3];
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
        .unwrap();
    assert_eq!(inode.blocks(), (TEST_BLOCK_SIZE / 512 * 3) as u64);
    install_test_internal_journal(&mut filesystem, 742);
    let free_before_truncate = filesystem.free_blocks_count();
    let new_size = u64::try_from(TEST_BLOCK_SIZE + 17).unwrap();

    filesystem
        .truncate_regular_inode(&inode, new_size, crate::Ext4Timestamp::new(22, 0))
        .unwrap();

    assert_eq!(inode.size(), new_size);
    assert_eq!(inode.blocks(), (TEST_BLOCK_SIZE / 512 * 2) as u64);
    assert_eq!(filesystem.orphan_head(), None);
    assert_eq!(
        filesystem.map_blocks(&inode, LogicalBlock::new(2)),
        Ok(crate::BlockMapping::Hole {
            len: BlockCount::new(u32::MAX),
            flags: crate::BlockMappingFlags::empty(),
        })
    );
    assert_eq!(filesystem.free_blocks_count(), free_before_truncate + 1);

    let crate::BlockMapping::Mapped { physical, .. } =
        filesystem.map_blocks(&inode, LogicalBlock::new(1)).unwrap()
    else {
        panic!("partial EOF block should remain mapped");
    };
    let mut eof_block = vec![0; TEST_BLOCK_SIZE];
    filesystem
        .read_blocks(FilesystemBlock::new(physical.get()), 1, &mut eof_block)
        .unwrap();
    assert_eq!(&eof_block[..17], &[0xa5; 17]);
    assert!(eof_block[17..].iter().all(|byte| *byte == 0));
}

/// Final-close discard drops only initialized mappings beyond `i_disksize`;
/// unwritten preallocation stays.
#[def_test]
fn preallocation_discard_drops_only_initialized_beyond_disk_size() {
    let fx = fallocate_vfs(TEST_FREE_BLOCKS, 0b0011_1111, 752);
    let inode = fx.inode_arc();
    let input = vec![0x42; TEST_BLOCK_SIZE * 4];
    let free_before_write = fx.free_blocks();

    inode.set_size(input.len() as u64);
    fx.ext4()
        .write(fx.super_block.as_ref())
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
    // The unwritten reservation beyond the committed disk size comes from
    // fallocate with KEEP_SIZE, not from in-memory writeback surplus.
    fx.file
        .fallocate(
            kvfs::FallocateFlags::KEEP_SIZE,
            input.len() as u64,
            TEST_BLOCK_SIZE as u64 * 4,
        )
        .unwrap();
    let BlockMapping::Unwritten {
        physical: unwritten_physical,
        ..
    } = fx.ext4().read().map_blocks(&inode, LogicalBlock::new(4)).unwrap()
    else {
        panic!("fallocate reservation should be unwritten");
    };

    // Crash-recovery leftover: an initialized mapping beyond the disk size.
    {
        let mut guard = fx.ext4().write(fx.super_block.as_ref());
        let credits = JournalCredits::new(64);
        let journal = guard
            .metadata_journal_for_mutation(
                credits,
                crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
            )
            .unwrap();
        let mut handle = journal.begin(credits).unwrap();
        let stale_physical = allocate_contiguous_blocks(&mut guard, &inode, 2, &mut handle);
        let result = guard.insert_extent_mapping(
            &inode,
            LogicalBlock::new(8),
            stale_physical,
            BlockCount::new(2),
            ExtentMappingState::Initialized,
            &mut handle,
        );
        guard.complete_metadata_mutation(handle, result).unwrap();
    }

    fx.ext4()
        .write(fx.super_block.as_ref())
        .discard_regular_inode_preallocations(&inode)
        .unwrap();

    assert_eq!(
        fx.ext4().read().map_blocks(&inode, LogicalBlock::new(8)),
        Ok(BlockMapping::Hole {
            len: BlockCount::new(u32::MAX),
            flags: BlockMappingFlags::empty(),
        })
    );
    assert_eq!(
        fx.ext4().read().map_blocks(&inode, LogicalBlock::new(4)),
        Ok(BlockMapping::Unwritten {
            physical: unwritten_physical,
            len: BlockCount::new(4),
            flags: BlockMappingFlags::empty(),
        })
    );
    assert_eq!(inode.blocks(), (TEST_BLOCK_SIZE / 512 * 8) as u64);
    assert_eq!(fx.free_blocks(), free_before_write - 8);
}

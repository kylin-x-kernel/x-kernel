// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Component invariants exercised in the kernel with synthetic metadata.

use alloc::vec::Vec;

use unittest::def_test;

use crate::{
    BlockCount, FilesystemBlock, LogicalBlock, PhysicalBlock,
    extent::ExtentMappingState,
    inode::InodeInitialization,
    jbd2::{JournalCredits, JournalTransactions, TransactionId},
    superblock::test_fixtures::{
        AllocatorGroupSpec, TEST_BLOCK_SIZE, TEST_FREE_BLOCKS, TEST_FREE_INODES,
        allocate_contiguous_blocks, allocator_multigroup_test_filesystem,
        allocator_test_filesystem, le_u16, le_u32,
    },
};

#[def_test]
fn inline_extent_mutation_journals_inode_table_update() {
    let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let journal = JournalTransactions::new(TransactionId::new(451));
    let mut handle = journal.begin(JournalCredits::new(8)).unwrap();

    let block = filesystem.allocate_block(None, &mut handle).unwrap();
    let inode_allocation = filesystem
        .allocate_inode(
            None,
            InodeInitialization::regular_file(0o644, 0, 0),
            &mut handle,
        )
        .unwrap();
    let inode = filesystem
        .internal_iget(inode_allocation.inode())
        .expect("read initialized inode");

    filesystem
        .insert_inline_extent_mapping(
            &inode,
            LogicalBlock::new(0),
            block.block(),
            BlockCount::new(1),
            ExtentMappingState::Initialized,
            &mut handle,
        )
        .expect("insert inline extent mapping");

    assert_eq!(
        filesystem.map_blocks(&inode, LogicalBlock::new(0)),
        Ok(crate::BlockMapping::Mapped {
            physical: block.block(),
            len: BlockCount::new(1),
            flags: crate::BlockMappingFlags::empty(),
        })
    );
    assert_eq!(handle.remaining_credits(), 3);
}

#[def_test]
fn extent_root_grow_without_revoke_journals_only_live_leaf_block() {
    let (mut filesystem, device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let journal = JournalTransactions::new(TransactionId::new(461));
    let mut handle = journal.begin(JournalCredits::new(64)).unwrap();
    let transaction = handle.id();
    let inode_allocation = filesystem
        .allocate_inode(
            None,
            InodeInitialization::regular_file(0o644, 0, 0),
            &mut handle,
        )
        .unwrap();
    let inode = filesystem
        .internal_iget(inode_allocation.inode())
        .expect("read initialized inode");
    let mut extents = Vec::new();

    for logical in [0, 2, 4, 6, 8, 10] {
        let block = filesystem.allocate_block(None, &mut handle).unwrap();
        filesystem
            .insert_extent_mapping(
                &inode,
                LogicalBlock::new(logical),
                block.block(),
                BlockCount::new(1),
                ExtentMappingState::Initialized,
                &mut handle,
            )
            .expect("insert extent mapping");
        extents.push((logical, block.block()));
    }

    assert_eq!(le_u16(&inode.raw_i_block(), 0x02), 1);
    assert_eq!(le_u16(&inode.raw_i_block(), 0x06), 1);
    let index_offset = crate::disk::extent::EXTENT_HEADER_SIZE;
    assert_eq!(le_u32(&inode.raw_i_block(), index_offset), 0);
    let extent_block = u64::from(le_u32(&inode.raw_i_block(), index_offset + 0x04))
        | (u64::from(le_u16(&inode.raw_i_block(), index_offset + 0x08)) << 32);
    // Extent tree blocks are no longer added to system_zones after the
    // system_zones ro refactor — only pre-existing mkfs-time zones remain.

    for (logical, physical) in extents {
        assert_eq!(
            filesystem.map_blocks(&inode, LogicalBlock::new(logical)),
            Ok(crate::BlockMapping::Mapped {
                physical,
                len: BlockCount::new(1),
                flags: crate::BlockMappingFlags::empty(),
            })
        );
    }
    drop(handle);

    let commit = journal.force_commit(transaction).unwrap();
    assert!(commit.revoked_blocks().unwrap().as_ref().is_empty());
    assert!(
        commit
            .metadata_blocks()
            .unwrap()
            .as_ref()
            .contains(&FilesystemBlock::new(extent_block))
    );
    filesystem
        .metadata_io
        .checkpoint_committed(&commit)
        .unwrap();
    journal.finish_checkpoint_for_test(&commit).unwrap();

    let bytes = device.bytes();
    let extent_block_offset = usize::try_from(extent_block).unwrap() * TEST_BLOCK_SIZE;
    assert_eq!(
        le_u16(&bytes, extent_block_offset),
        crate::disk::extent::EXTENT_MAGIC
    );
    assert_eq!(le_u16(&bytes, extent_block_offset + 0x02), 6);
    assert_eq!(le_u16(&bytes, extent_block_offset + 0x06), 0);
}

#[def_test]
fn extent_point_mutations_keep_existing_leaf_block() {
    let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let journal = JournalTransactions::new(TransactionId::new(4611));
    let mut handle = journal.begin(JournalCredits::new(256)).unwrap();
    let inode_allocation = filesystem
        .allocate_inode(
            None,
            InodeInitialization::regular_file(0o644, 0, 0),
            &mut handle,
        )
        .unwrap();
    let inode = filesystem
        .internal_iget(inode_allocation.inode())
        .expect("read initialized inode");

    for logical in [0, 2, 4, 6, 8] {
        let block = filesystem.allocate_block(None, &mut handle).unwrap();
        filesystem
            .insert_extent_mapping(
                &inode,
                LogicalBlock::new(logical),
                block.block(),
                BlockCount::new(1),
                ExtentMappingState::Initialized,
                &mut handle,
            )
            .unwrap();
    }
    let index_offset = crate::disk::extent::EXTENT_HEADER_SIZE;
    let extent_block = u64::from(le_u32(&inode.raw_i_block(), index_offset + 0x04))
        | (u64::from(le_u16(&inode.raw_i_block(), index_offset + 0x08)) << 32);

    let data = filesystem.allocate_block(None, &mut handle).unwrap();
    filesystem
        .insert_extent_mapping(
            &inode,
            LogicalBlock::new(10),
            data.block(),
            BlockCount::new(1),
            ExtentMappingState::Unwritten,
            &mut handle,
        )
        .unwrap();
    filesystem
        .convert_unwritten_extent_range(
            &inode,
            LogicalBlock::new(10),
            BlockCount::new(1),
            &mut handle,
        )
        .unwrap();
    filesystem
        .remove_extent_range(
            &inode,
            LogicalBlock::new(10),
            BlockCount::new(1),
            &mut handle,
        )
        .unwrap();

    let current_extent_block = u64::from(le_u32(&inode.raw_i_block(), index_offset + 0x04))
        | (u64::from(le_u16(&inode.raw_i_block(), index_offset + 0x08)) << 32);
    assert_eq!(current_extent_block, extent_block);
    assert_eq!(
        filesystem.map_blocks(&inode, LogicalBlock::new(10)),
        Ok(crate::BlockMapping::Hole {
            len: BlockCount::new(u32::MAX),
            flags: crate::BlockMappingFlags::empty(),
        })
    );
}

#[def_test]
fn extent_leaf_split_leaves_space_in_reused_leaf() {
    let mut groups = [AllocatorGroupSpec {
        free_blocks: TEST_FREE_BLOCKS,
        free_inodes: TEST_FREE_INODES,
        used_directories: 0,
        flags: 0,
        block_bitmap: [0b0011_1111, 0, 0, 0],
        inode_bitmap: [0, 0, 0, 0],
    }; 16];
    groups[0].inode_bitmap = [0xff, 0x03, 0, 0];
    let (mut filesystem, _device) = allocator_multigroup_test_filesystem(&groups);
    let journal = JournalTransactions::new(TransactionId::new(4612));
    let mut handle = journal.begin(JournalCredits::new(100_000)).unwrap();
    let inode_allocation = filesystem
        .allocate_inode(
            None,
            InodeInitialization::regular_file(0o644, 0, 0),
            &mut handle,
        )
        .unwrap();
    let inode = filesystem
        .internal_iget(inode_allocation.inode())
        .expect("read initialized inode");
    let leaf_capacity =
        u64::from(crate::disk::extent::extent_block_capacity(TEST_BLOCK_SIZE).unwrap());

    for logical in 0..=leaf_capacity {
        let block = filesystem.allocate_block(None, &mut handle).unwrap();
        filesystem
            .insert_extent_mapping(
                &inode,
                LogicalBlock::new(logical * 2),
                block.block(),
                BlockCount::new(1),
                ExtentMappingState::Initialized,
                &mut handle,
            )
            .unwrap();
    }
    assert_eq!(le_u16(&inode.raw_i_block(), 0x02), 2);

    let inserted = filesystem.allocate_block(None, &mut handle).unwrap();
    filesystem
        .insert_extent_mapping(
            &inode,
            LogicalBlock::new(1),
            inserted.block(),
            BlockCount::new(1),
            ExtentMappingState::Initialized,
            &mut handle,
        )
        .unwrap();

    assert_eq!(le_u16(&inode.raw_i_block(), 0x02), 2);
    assert_eq!(
        filesystem.map_blocks(&inode, LogicalBlock::new(1)),
        Ok(crate::BlockMapping::Mapped {
            physical: inserted.block(),
            len: BlockCount::new(1),
            flags: crate::BlockMappingFlags::empty(),
        })
    );
}

#[def_test]
fn extent_unwritten_conversion_splits_only_requested_range() {
    let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let journal = JournalTransactions::new(TransactionId::new(462));
    let mut handle = journal.begin(JournalCredits::new(256)).unwrap();
    let physical = allocate_contiguous_blocks(&mut filesystem, 6, &mut handle);
    let inode_allocation = filesystem
        .allocate_inode(
            None,
            InodeInitialization::regular_file(0o644, 0, 0),
            &mut handle,
        )
        .unwrap();
    let inode = filesystem.internal_iget(inode_allocation.inode()).unwrap();

    filesystem
        .insert_extent_mapping(
            &inode,
            LogicalBlock::new(0),
            physical,
            BlockCount::new(6),
            ExtentMappingState::Unwritten,
            &mut handle,
        )
        .unwrap();
    filesystem
        .convert_unwritten_extent_range(
            &inode,
            LogicalBlock::new(2),
            BlockCount::new(2),
            &mut handle,
        )
        .unwrap();

    assert_eq!(
        filesystem.map_blocks(&inode, LogicalBlock::new(0)),
        Ok(crate::BlockMapping::Unwritten {
            physical,
            len: BlockCount::new(2),
            flags: crate::BlockMappingFlags::empty(),
        })
    );
    assert_eq!(
        filesystem.map_blocks(&inode, LogicalBlock::new(2)),
        Ok(crate::BlockMapping::Mapped {
            physical: PhysicalBlock::new(physical.get() + 2),
            len: BlockCount::new(2),
            flags: crate::BlockMappingFlags::empty(),
        })
    );
    assert_eq!(
        filesystem.map_blocks(&inode, LogicalBlock::new(4)),
        Ok(crate::BlockMapping::Unwritten {
            physical: PhysicalBlock::new(physical.get() + 4),
            len: BlockCount::new(2),
            flags: crate::BlockMappingFlags::empty(),
        })
    );
}

#[def_test]
fn allocation_goal_after_previous_extent_uses_requested_logical_block() {
    let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let journal = JournalTransactions::new(TransactionId::new(463));
    let mut handle = journal.begin(JournalCredits::new(128)).unwrap();
    let physical = allocate_contiguous_blocks(&mut filesystem, 10, &mut handle);
    let inode_allocation = filesystem
        .allocate_inode(
            None,
            InodeInitialization::regular_file(0o644, 0, 0),
            &mut handle,
        )
        .unwrap();
    let inode = filesystem.internal_iget(inode_allocation.inode()).unwrap();
    filesystem
        .insert_extent_mapping(
            &inode,
            LogicalBlock::new(10),
            physical,
            BlockCount::new(10),
            ExtentMappingState::Initialized,
            &mut handle,
        )
        .unwrap();

    assert_eq!(
        filesystem
            .allocation_goal_after_previous_extent(&inode, LogicalBlock::new(15))
            .unwrap(),
        Some(FilesystemBlock::new(physical.get() + 5))
    );
}

#[def_test]
fn extent_remove_range_splits_mapping_and_releases_data_blocks() {
    let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let journal = JournalTransactions::new(TransactionId::new(463));
    let mut handle = journal.begin(JournalCredits::new(256)).unwrap();
    let physical = allocate_contiguous_blocks(&mut filesystem, 6, &mut handle);
    let inode_allocation = filesystem
        .allocate_inode(
            None,
            InodeInitialization::regular_file(0o644, 0, 0),
            &mut handle,
        )
        .unwrap();
    let inode = filesystem.internal_iget(inode_allocation.inode()).unwrap();
    let free_after_allocation = filesystem.free_blocks_count();

    filesystem
        .insert_extent_mapping(
            &inode,
            LogicalBlock::new(0),
            physical,
            BlockCount::new(6),
            ExtentMappingState::Initialized,
            &mut handle,
        )
        .unwrap();
    filesystem
        .remove_extent_range(
            &inode,
            LogicalBlock::new(2),
            BlockCount::new(2),
            &mut handle,
        )
        .unwrap();

    assert_eq!(
        filesystem.map_blocks(&inode, LogicalBlock::new(0)),
        Ok(crate::BlockMapping::Mapped {
            physical,
            len: BlockCount::new(2),
            flags: crate::BlockMappingFlags::empty(),
        })
    );
    assert_eq!(
        filesystem.map_blocks(&inode, LogicalBlock::new(2)),
        Ok(crate::BlockMapping::Hole {
            len: BlockCount::new(2),
            flags: crate::BlockMappingFlags::empty(),
        })
    );
    assert_eq!(
        filesystem.map_blocks(&inode, LogicalBlock::new(4)),
        Ok(crate::BlockMapping::Mapped {
            physical: PhysicalBlock::new(physical.get() + 4),
            len: BlockCount::new(2),
            flags: crate::BlockMappingFlags::empty(),
        })
    );
    assert_eq!(filesystem.free_blocks_count(), free_after_allocation + 2);
}

#[def_test]
fn extent_remove_range_splits_full_inline_leaf() {
    let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let journal = JournalTransactions::new(TransactionId::new(4631));
    let mut handle = journal.begin(JournalCredits::new(256)).unwrap();
    let physical = allocate_contiguous_blocks(&mut filesystem, 3, &mut handle);
    let inode_allocation = filesystem
        .allocate_inode(
            None,
            InodeInitialization::regular_file(0o644, 0, 0),
            &mut handle,
        )
        .unwrap();
    let inode = filesystem.internal_iget(inode_allocation.inode()).unwrap();
    filesystem
        .insert_extent_mapping(
            &inode,
            LogicalBlock::new(0),
            physical,
            BlockCount::new(3),
            ExtentMappingState::Initialized,
            &mut handle,
        )
        .unwrap();
    for logical in [4, 6, 8] {
        let block = filesystem.allocate_block(None, &mut handle).unwrap();
        filesystem
            .insert_extent_mapping(
                &inode,
                LogicalBlock::new(logical),
                block.block(),
                BlockCount::new(1),
                ExtentMappingState::Initialized,
                &mut handle,
            )
            .unwrap();
    }

    filesystem
        .remove_extent_range(
            &inode,
            LogicalBlock::new(1),
            BlockCount::new(1),
            &mut handle,
        )
        .unwrap();

    assert_eq!(le_u16(&inode.raw_i_block(), 0x06), 1);
    assert_eq!(
        filesystem.map_blocks(&inode, LogicalBlock::new(1)),
        Ok(crate::BlockMapping::Hole {
            len: BlockCount::new(1),
            flags: crate::BlockMappingFlags::empty(),
        })
    );
    assert_eq!(
        filesystem.map_blocks(&inode, LogicalBlock::new(2)),
        Ok(crate::BlockMapping::Mapped {
            physical: PhysicalBlock::new(physical.get() + 2),
            len: BlockCount::new(1),
            flags: crate::BlockMappingFlags::empty(),
        })
    );
}

#[def_test]
fn extent_truncate_releases_tail_range() {
    let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let journal = JournalTransactions::new(TransactionId::new(464));
    let mut handle = journal.begin(JournalCredits::new(256)).unwrap();
    let physical = allocate_contiguous_blocks(&mut filesystem, 6, &mut handle);
    let inode_allocation = filesystem
        .allocate_inode(
            None,
            InodeInitialization::regular_file(0o644, 0, 0),
            &mut handle,
        )
        .unwrap();
    let inode = filesystem.internal_iget(inode_allocation.inode()).unwrap();
    let free_after_allocation = filesystem.free_blocks_count();

    filesystem
        .insert_extent_mapping(
            &inode,
            LogicalBlock::new(0),
            physical,
            BlockCount::new(6),
            ExtentMappingState::Initialized,
            &mut handle,
        )
        .unwrap();
    filesystem
        .truncate_extent_mappings(&inode, LogicalBlock::new(3), &mut handle)
        .unwrap();

    assert_eq!(
        filesystem.map_blocks(&inode, LogicalBlock::new(0)),
        Ok(crate::BlockMapping::Mapped {
            physical,
            len: BlockCount::new(3),
            flags: crate::BlockMappingFlags::empty(),
        })
    );
    assert_eq!(
        filesystem.map_blocks(&inode, LogicalBlock::new(3)),
        Ok(crate::BlockMapping::Hole {
            len: BlockCount::new(u32::MAX),
            flags: crate::BlockMappingFlags::empty(),
        })
    );
    assert_eq!(filesystem.free_blocks_count(), free_after_allocation + 3);
}

#[def_test]
fn extent_mutation_splits_multiple_leaf_blocks() {
    let mut groups = [AllocatorGroupSpec {
        free_blocks: TEST_FREE_BLOCKS,
        free_inodes: TEST_FREE_INODES,
        used_directories: 0,
        flags: 0,
        block_bitmap: [0b0011_1111, 0, 0, 0],
        inode_bitmap: [0, 0, 0, 0],
    }; 16];
    groups[0].inode_bitmap = [0xff, 0x03, 0, 0];
    let (mut filesystem, _device) = allocator_multigroup_test_filesystem(&groups);
    let journal = JournalTransactions::new(TransactionId::new(465));
    let mut handle = journal.begin(JournalCredits::new(100_000)).unwrap();
    let inode_allocation = filesystem
        .allocate_inode(
            None,
            InodeInitialization::regular_file(0o644, 0, 0),
            &mut handle,
        )
        .unwrap();
    let inode = filesystem.internal_iget(inode_allocation.inode()).unwrap();
    let mut mappings = Vec::new();

    for logical in 0..350u64 {
        let block = filesystem.allocate_block(None, &mut handle).unwrap();
        filesystem
            .insert_extent_mapping(
                &inode,
                LogicalBlock::new(logical * 2),
                block.block(),
                BlockCount::new(1),
                ExtentMappingState::Initialized,
                &mut handle,
            )
            .unwrap();
        mappings.push((logical * 2, block.block()));
    }

    assert_eq!(le_u16(&inode.raw_i_block(), 0x02), 2);
    assert_eq!(le_u16(&inode.raw_i_block(), 0x06), 1);
    for (logical, physical) in mappings {
        assert_eq!(
            filesystem.map_blocks(&inode, LogicalBlock::new(logical)),
            Ok(crate::BlockMapping::Mapped {
                physical,
                len: BlockCount::new(1),
                flags: crate::BlockMappingFlags::empty(),
            })
        );
    }
}

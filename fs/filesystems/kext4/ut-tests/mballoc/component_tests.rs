// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Component invariants exercised in the kernel with synthetic metadata.

use unittest::def_test;

use crate::{
    BlockCount, BlockGroupNumber, Ext4Error, FilesystemBlock, LogicalBlock, PhysicalBlock,
    jbd2::{JournalCredits, JournalTransactions, TransactionId},
    mballoc::{Ext4AllocationFlags, Ext4AllocationRequest},
    superblock::test_fixtures::{
        AllocatorGroupSpec, TEST_BLOCK_COUNT, TEST_BLOCK_SIZE, TEST_EXT4_BG_BLOCK_UNINIT,
        TEST_FREE_BLOCKS, TEST_FREE_INODES, allocator_multigroup_test_filesystem,
        allocator_test_filesystem, le_u16, le_u32,
    },
};

#[def_test]
fn mballoc_allocates_goal_aligned_contiguous_run() {
    let (filesystem, device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let journal = JournalTransactions::new(TransactionId::new(111));
    let mut handle = journal.begin(JournalCredits::new(4)).unwrap();
    let transaction = handle.id();
    let request = Ext4AllocationRequest::new(
        LogicalBlock::new(7),
        Some(FilesystemBlock::new(8)),
        BlockCount::new(4),
        BlockCount::new(4),
        Ext4AllocationFlags::EXACT,
        BlockGroupNumber::new(0),
    )
    .unwrap();

    let allocation = filesystem
        .allocate_blocks_for_write(request, &mut handle)
        .unwrap();

    assert_eq!(allocation.logical_start(), LogicalBlock::new(7));
    assert_eq!(allocation.group(), BlockGroupNumber::new(0));
    assert_eq!(allocation.physical_start(), PhysicalBlock::new(8));
    assert_eq!(allocation.block_count(), BlockCount::new(4));
    assert_eq!(allocation.requested_len(), BlockCount::new(4));
    assert!(!allocation.is_partial());
    assert_eq!(filesystem.groups()[0].free_blocks_count(), 22);
    assert_eq!(filesystem.free_blocks_count(), 22);
    drop(handle);

    let commit = journal.force_commit(transaction).unwrap();
    assert_eq!(commit.used_credits().unwrap(), 3);
    filesystem
        .metadata_io
        .checkpoint_committed(&commit)
        .unwrap();
    journal.finish_checkpoint_for_test(&commit).unwrap();

    let bytes = device.bytes();
    assert_eq!(bytes[2 * TEST_BLOCK_SIZE + 1] & 0b0000_1111, 0b0000_1111);
    assert_eq!(le_u16(&bytes, TEST_BLOCK_SIZE + 12), 22);
    assert_eq!(le_u32(&bytes, 1024 + 0x0c), 22);
}

#[def_test]
fn mballoc_returns_explicit_partial_run_when_fragmented() {
    let (filesystem, _device) = allocator_multigroup_test_filesystem(&[AllocatorGroupSpec {
        free_blocks: 2,
        free_inodes: TEST_FREE_INODES,
        used_directories: 0,
        flags: 0,
        block_bitmap: [0xff, 0b1111_1100, 0xff, 0xff],
        inode_bitmap: [0xff, 0x03, 0, 0],
    }]);
    let journal = JournalTransactions::new(TransactionId::new(112));
    let mut handle = journal.begin(JournalCredits::new(3)).unwrap();
    let request = Ext4AllocationRequest::new(
        LogicalBlock::new(11),
        Some(FilesystemBlock::new(8)),
        BlockCount::new(4),
        BlockCount::new(2),
        Ext4AllocationFlags::ALLOW_PARTIAL,
        BlockGroupNumber::new(0),
    )
    .unwrap();

    let allocation = filesystem
        .allocate_blocks_for_write(request, &mut handle)
        .unwrap();

    assert_eq!(allocation.physical_start(), PhysicalBlock::new(8));
    assert_eq!(allocation.block_count(), BlockCount::new(2));
    assert_eq!(allocation.requested_len(), BlockCount::new(4));
    assert!(allocation.is_partial());
    assert_eq!(filesystem.groups()[0].free_blocks_count(), 0);
    assert_eq!(filesystem.free_blocks_count(), 0);
}

#[def_test]
fn mballoc_falls_back_when_group_cannot_satisfy_minimum_run() {
    let (filesystem, _device) = allocator_multigroup_test_filesystem(&[
        AllocatorGroupSpec {
            free_blocks: 1,
            free_inodes: TEST_FREE_INODES,
            used_directories: 0,
            flags: 0,
            block_bitmap: [0b1011_1111, 0xff, 0xff, 0xff],
            inode_bitmap: [0xff, 0x03, 0, 0],
        },
        AllocatorGroupSpec {
            free_blocks: TEST_FREE_BLOCKS,
            free_inodes: TEST_FREE_INODES,
            used_directories: 0,
            flags: 0,
            block_bitmap: [0b0011_1111, 0, 0, 0],
            inode_bitmap: [0, 0, 0, 0],
        },
    ]);
    let journal = JournalTransactions::new(TransactionId::new(113));
    let mut handle = journal.begin(JournalCredits::new(3)).unwrap();
    let request = Ext4AllocationRequest::new(
        LogicalBlock::new(12),
        None,
        BlockCount::new(2),
        BlockCount::new(2),
        Ext4AllocationFlags::EXACT,
        BlockGroupNumber::new(0),
    )
    .unwrap();

    let allocation = filesystem
        .allocate_blocks_for_write(request, &mut handle)
        .unwrap();

    assert_eq!(allocation.group(), BlockGroupNumber::new(1));
    assert_eq!(allocation.physical_start(), PhysicalBlock::new(38));
    assert_eq!(allocation.block_count(), BlockCount::new(2));
    assert_eq!(filesystem.groups()[0].free_blocks_count(), 1);
    assert_eq!(filesystem.groups()[1].free_blocks_count(), 24);
}

#[def_test]
fn mballoc_rejects_exact_request_with_smaller_minimum() {
    assert_eq!(
        Ext4AllocationRequest::new(
            LogicalBlock::new(13),
            None,
            BlockCount::new(4),
            BlockCount::new(2),
            Ext4AllocationFlags::EXACT,
            BlockGroupNumber::new(0),
        ),
        Err(Ext4Error::OutOfBounds)
    );
}

#[def_test]
fn mballoc_run_allocation_lazy_initializes_nonzero_group_bitmap() {
    let (filesystem, device) = allocator_multigroup_test_filesystem(&[
        AllocatorGroupSpec {
            free_blocks: 0,
            free_inodes: 0,
            used_directories: 0,
            flags: 0,
            block_bitmap: [0xff; 4],
            inode_bitmap: [0xff; 4],
        },
        AllocatorGroupSpec {
            free_blocks: 32,
            free_inodes: 0,
            used_directories: 0,
            flags: TEST_EXT4_BG_BLOCK_UNINIT,
            block_bitmap: [0; 4],
            inode_bitmap: [0xff; 4],
        },
    ]);
    let journal = JournalTransactions::new(TransactionId::new(334));
    let mut handle = journal.begin(JournalCredits::new(3)).unwrap();
    let transaction = handle.id();
    let request = Ext4AllocationRequest::new(
        LogicalBlock::new(20),
        Some(FilesystemBlock::new(38)),
        BlockCount::new(4),
        BlockCount::new(4),
        Ext4AllocationFlags::EXACT,
        BlockGroupNumber::new(1),
    )
    .unwrap();

    let allocation = filesystem
        .allocate_blocks_for_write(request, &mut handle)
        .unwrap();

    assert_eq!(allocation.group(), BlockGroupNumber::new(1));
    assert_eq!(allocation.physical_start(), PhysicalBlock::new(38));
    assert_eq!(allocation.block_count(), BlockCount::new(4));
    assert_eq!(filesystem.groups()[1].free_blocks_count(), 22);
    assert_eq!(filesystem.groups()[1].flags(), 0);
    assert_eq!(filesystem.free_blocks_count(), 22);
    drop(handle);

    let commit = journal.force_commit(transaction).unwrap();
    filesystem
        .metadata_io
        .checkpoint_committed(&commit)
        .unwrap();
    journal.finish_checkpoint_for_test(&commit).unwrap();

    let bytes = device.bytes();
    let group1_descriptor_offset = TEST_BLOCK_SIZE + 64;
    let group1_block_bitmap_offset = (TEST_BLOCK_COUNT + 2) * TEST_BLOCK_SIZE;
    assert_eq!(
        le_u16(&bytes, group1_descriptor_offset + 18) & TEST_EXT4_BG_BLOCK_UNINIT,
        0
    );
    assert_eq!(le_u16(&bytes, group1_descriptor_offset + 12), 22);
    assert_eq!(bytes[group1_block_bitmap_offset], 0xff);
    assert_eq!(
        bytes[group1_block_bitmap_offset + 1] & 0b0000_0011,
        0b0000_0011
    );
    assert_eq!(le_u32(&bytes, 1024 + 0x0c), 22);
}

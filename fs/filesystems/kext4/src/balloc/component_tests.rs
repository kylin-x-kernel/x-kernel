// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Component invariants exercised in the kernel with synthetic metadata.

use unittest::def_test;

use crate::{
    BlockGroupNumber, CorruptKind, Ext4Error, FilesystemBlock, PhysicalBlock,
    jbd2::{JournalCredits, JournalTransactions, TransactionId},
    superblock::{
        ext4_bitmap_checksum_matches, ext4_mark_bitmap_end,
        test_fixtures::{
            AllocatorGroupSpec, TEST_BLOCK_COUNT, TEST_BLOCK_SIZE, TEST_EXT4_BG_BLOCK_UNINIT,
            TEST_FREE_BLOCKS, TEST_FREE_INODES, allocator_multigroup_test_filesystem,
            allocator_test_filesystem, allocator_test_filesystem_with_flags,
            allocator_test_filesystem_with_options, le_u16, le_u32,
        },
    },
};

#[def_test]
fn block_allocator_uses_goal_group_and_falls_back_across_groups() {
    let (filesystem, _device) = allocator_multigroup_test_filesystem(&[
        AllocatorGroupSpec {
            free_blocks: 0,
            free_inodes: TEST_FREE_INODES,
            used_directories: 0,
            flags: 0,
            block_bitmap: [0xff; 4],
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
    let journal = JournalTransactions::new(TransactionId::new(801));
    let mut handle = journal.begin(JournalCredits::new(4)).unwrap();

    let allocation = filesystem
        .allocate_block(Some(FilesystemBlock::new(8)), &mut handle)
        .unwrap();

    assert_eq!(allocation.group(), BlockGroupNumber::new(1));
    assert_eq!(allocation.block(), PhysicalBlock::new(38));
}

#[def_test]
fn block_allocator_starts_scan_at_goal_inside_selected_group() {
    let (filesystem, _device) = allocator_multigroup_test_filesystem(&[
        AllocatorGroupSpec {
            free_blocks: TEST_FREE_BLOCKS,
            free_inodes: TEST_FREE_INODES,
            used_directories: 0,
            flags: 0,
            block_bitmap: [0b0011_1111, 0, 0, 0],
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
    let journal = JournalTransactions::new(TransactionId::new(811));
    let mut handle = journal.begin(JournalCredits::new(3)).unwrap();

    let allocation = filesystem
        .allocate_block(Some(FilesystemBlock::new(40)), &mut handle)
        .unwrap();

    assert_eq!(allocation.group(), BlockGroupNumber::new(1));
    assert_eq!(allocation.block(), PhysicalBlock::new(40));
    assert_eq!(allocation.bitmap_bit(), 8);
}

#[def_test]
fn block_allocator_journals_bitmap_group_and_superblock_updates() {
    let (filesystem, device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let journal = JournalTransactions::new(TransactionId::new(101));
    let mut handle = journal.begin(JournalCredits::new(4)).unwrap();
    let transaction = handle.id();

    let allocation = filesystem
        .allocate_block_in_group(BlockGroupNumber::new(0), None, &mut handle)
        .unwrap();

    assert_eq!(allocation.block(), PhysicalBlock::new(6));
    assert_eq!(allocation.bitmap_bit(), 6);
    assert_eq!(filesystem.groups()[0].free_blocks_count(), 25);
    assert_eq!(filesystem.free_blocks_count(), 25);
    // Block allocation does not touch the delayed-allocation reserve.
    assert_eq!(filesystem.delalloc_reserved_block_count(), 0);
    drop(handle);

    let commit = journal.force_commit(transaction).unwrap();
    assert_eq!(commit.used_credits().unwrap(), 3);
    assert_eq!(
        commit.metadata_blocks().unwrap().as_ref(),
        &[
            FilesystemBlock::new(0),
            FilesystemBlock::new(1),
            FilesystemBlock::new(2),
        ]
    );

    filesystem
        .metadata_io
        .checkpoint_committed(&commit)
        .unwrap();
    journal.finish_checkpoint_for_test(&commit).unwrap();

    let bytes = device.bytes();
    assert_eq!(bytes[2 * TEST_BLOCK_SIZE] & 0b0100_0000, 0b0100_0000);
    assert_eq!(le_u16(&bytes, TEST_BLOCK_SIZE + 12), 25);
    assert_eq!(le_u32(&bytes, 1024 + 0x0c), 25);
}

#[def_test]
fn block_allocator_releases_allocated_block_through_same_metadata_path() {
    let (filesystem, device) = allocator_test_filesystem(25, 0b0111_1111);
    let journal = JournalTransactions::new(TransactionId::new(201));
    let mut handle = journal.begin(JournalCredits::new(3)).unwrap();
    let transaction = handle.id();

    let released = filesystem
        .release_allocated_block(PhysicalBlock::new(6), &mut handle)
        .unwrap();

    assert_eq!(released.block(), PhysicalBlock::new(6));
    assert_eq!(released.bitmap_bit(), 6);
    assert_eq!(filesystem.groups()[0].free_blocks_count(), 26);
    assert_eq!(filesystem.free_blocks_count(), 26);
    drop(handle);

    let commit = journal.force_commit(transaction).unwrap();
    filesystem
        .metadata_io
        .checkpoint_committed(&commit)
        .unwrap();
    journal.finish_checkpoint_for_test(&commit).unwrap();

    let bytes = device.bytes();
    assert_eq!(bytes[2 * TEST_BLOCK_SIZE] & 0b0100_0000, 0);
    assert_eq!(le_u16(&bytes, TEST_BLOCK_SIZE + 12), 26);
    assert_eq!(le_u32(&bytes, 1024 + 0x0c), 26);
}

#[def_test]
fn block_allocator_releases_metadata_block_with_revoke_record() {
    let (filesystem, device) = allocator_test_filesystem(25, 0b0111_1111);
    let journal = JournalTransactions::new(TransactionId::new(202));
    let mut handle = journal.begin(JournalCredits::new(4)).unwrap();
    let transaction = handle.id();

    let released = filesystem
        .release_allocated_metadata_block(PhysicalBlock::new(6), &mut handle)
        .unwrap();

    assert_eq!(released.block(), PhysicalBlock::new(6));
    assert_eq!(filesystem.groups()[0].free_blocks_count(), 26);
    drop(handle);

    let commit = journal.force_commit(transaction).unwrap();
    assert_eq!(commit.used_credits().unwrap(), 4);
    assert_eq!(
        commit.metadata_blocks().unwrap().as_ref(),
        &[
            FilesystemBlock::new(0),
            FilesystemBlock::new(1),
            FilesystemBlock::new(2),
        ]
    );
    assert_eq!(
        commit.revoked_blocks().unwrap().as_ref(),
        &[FilesystemBlock::new(6)]
    );

    filesystem
        .metadata_io
        .checkpoint_committed(&commit)
        .unwrap();
    journal.finish_checkpoint_for_test(&commit).unwrap();

    let bytes = device.bytes();
    assert_eq!(bytes[2 * TEST_BLOCK_SIZE] & 0b0100_0000, 0);
    assert_eq!(le_u16(&bytes, TEST_BLOCK_SIZE + 12), 26);
    assert_eq!(le_u32(&bytes, 1024 + 0x0c), 26);
}

#[def_test]
fn block_allocator_rejects_releasing_system_zone_without_consuming_credits() {
    let (filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let journal = JournalTransactions::new(TransactionId::new(301));
    let mut handle = journal.begin(JournalCredits::new(3)).unwrap();

    assert_eq!(
        filesystem.release_allocated_block(PhysicalBlock::new(2), &mut handle),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockBitmap))
    );
    assert_eq!(handle.remaining_credits(), 3);
}

#[def_test]
fn block_allocator_rejects_insufficient_credits_before_metadata_access() {
    let (filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let journal = JournalTransactions::new(TransactionId::new(311));
    let mut handle = journal.begin(JournalCredits::new(2)).unwrap();

    assert_eq!(
        filesystem.allocate_block(None, &mut handle),
        Err(Ext4Error::InsufficientJournalCredits)
    );
    assert_eq!(handle.remaining_credits(), 2);
    assert_eq!(filesystem.groups()[0].free_blocks_count(), TEST_FREE_BLOCKS);
    assert_eq!(filesystem.free_blocks_count(), u64::from(TEST_FREE_BLOCKS));
}

#[def_test]
fn ext4_bitmap_tail_bits_are_marked_used() {
    let mut bitmap = [0; 2];

    ext4_mark_bitmap_end(10, 16, &mut bitmap).unwrap();

    assert_eq!(bitmap, [0, 0b1111_1100]);
}

#[def_test]
fn bitmap_checksum_compare_uses_descriptor_width() {
    assert!(ext4_bitmap_checksum_matches(
        0xaaaa_1234,
        0xbbbb_1234,
        false
    ));
    assert!(!ext4_bitmap_checksum_matches(
        0xaaaa_1234,
        0xbbbb_5678,
        false
    ));
    assert!(!ext4_bitmap_checksum_matches(
        0xaaaa_1234,
        0xbbbb_1234,
        true
    ));
    assert!(ext4_bitmap_checksum_matches(0xaaaa_1234, 0xaaaa_1234, true));
}

#[def_test]
fn block_allocator_rejects_uninit_group_zero() {
    let (filesystem, _device) = allocator_test_filesystem_with_flags(
        TEST_FREE_BLOCKS,
        0,
        TEST_FREE_INODES,
        0x03,
        0,
        TEST_EXT4_BG_BLOCK_UNINIT,
    );
    let journal = JournalTransactions::new(TransactionId::new(321));
    let mut handle = journal.begin(JournalCredits::new(3)).unwrap();

    assert_eq!(
        filesystem.allocate_block_in_group(BlockGroupNumber::new(0), None, &mut handle),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockBitmap))
    );
    assert_eq!(handle.remaining_credits(), 3);
}

#[def_test]
fn block_allocator_lazy_initializes_nonzero_group_bitmap() {
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
    let journal = JournalTransactions::new(TransactionId::new(324));
    let mut handle = journal.begin(JournalCredits::new(3)).unwrap();
    let transaction = handle.id();

    let allocation = filesystem
        .allocate_block(Some(FilesystemBlock::new(38)), &mut handle)
        .unwrap();

    assert_eq!(allocation.group(), BlockGroupNumber::new(1));
    assert_eq!(allocation.block(), PhysicalBlock::new(38));
    assert_eq!(filesystem.groups()[1].free_blocks_count(), 25);
    assert_eq!(filesystem.groups()[1].flags(), 0);
    assert_eq!(filesystem.free_blocks_count(), 25);
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
    assert_eq!(le_u16(&bytes, group1_descriptor_offset + 12), 25);
    assert_eq!(bytes[group1_block_bitmap_offset], 0b0111_1111);
    assert_eq!(le_u32(&bytes, 1024 + 0x0c), 25);
}

#[def_test]
fn block_allocator_rejects_initialized_bitmap_with_cleared_tail_bit() {
    let (filesystem, device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    device.bytes.lock()[2 * TEST_BLOCK_SIZE + 4] = 0;
    let journal = JournalTransactions::new(TransactionId::new(325));
    let mut handle = journal.begin(JournalCredits::new(3)).unwrap();

    assert_eq!(
        filesystem.allocate_block_in_group(BlockGroupNumber::new(0), None, &mut handle),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockBitmap))
    );
    assert_eq!(
        filesystem.allocate_block(None, &mut handle),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockBitmap))
    );
    assert_eq!(handle.remaining_credits(), 3);
    assert_eq!(device.bytes.lock()[2 * TEST_BLOCK_SIZE + 4], 0);
}

#[def_test]
fn block_allocator_rejects_initialized_bitmap_with_cleared_system_zone_bit() {
    let (filesystem, device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1011);
    let journal = JournalTransactions::new(TransactionId::new(326));
    let mut handle = journal.begin(JournalCredits::new(3)).unwrap();

    assert_eq!(
        filesystem.allocate_block_in_group(BlockGroupNumber::new(0), None, &mut handle),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockBitmap))
    );
    assert_eq!(handle.remaining_credits(), 3);
    assert_eq!(device.bytes.lock()[2 * TEST_BLOCK_SIZE], 0b0011_1011);
}

#[def_test]
fn block_allocator_rejects_initialized_bitmap_checksum_mismatch() {
    let (filesystem, device) = allocator_test_filesystem_with_options(
        AllocatorGroupSpec {
            free_blocks: TEST_FREE_BLOCKS,
            free_inodes: TEST_FREE_INODES,
            used_directories: 0,
            flags: 0,
            block_bitmap: [0b0011_1111, 0, 0, 0],
            inode_bitmap: [0xff, 0x03, 0, 0],
        },
        true,
        true,
        false,
    );
    let journal = JournalTransactions::new(TransactionId::new(327));
    let mut handle = journal.begin(JournalCredits::new(3)).unwrap();

    assert_eq!(
        filesystem.allocate_block_in_group(BlockGroupNumber::new(0), None, &mut handle),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockBitmap))
    );
    assert_eq!(handle.remaining_credits(), 3);
    assert_eq!(device.bytes.lock()[2 * TEST_BLOCK_SIZE], 0b0011_1111);
}

#[def_test]
fn block_allocator_release_rejects_initialized_bitmap_checksum_mismatch() {
    let (filesystem, device) = allocator_test_filesystem_with_options(
        AllocatorGroupSpec {
            free_blocks: 25,
            free_inodes: TEST_FREE_INODES,
            used_directories: 0,
            flags: 0,
            block_bitmap: [0b0111_1111, 0, 0, 0],
            inode_bitmap: [0xff, 0x03, 0, 0],
        },
        true,
        true,
        false,
    );
    let journal = JournalTransactions::new(TransactionId::new(328));
    let mut handle = journal.begin(JournalCredits::new(3)).unwrap();

    assert_eq!(
        filesystem.release_allocated_block(PhysicalBlock::new(6), &mut handle),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockBitmap))
    );
    assert_eq!(handle.remaining_credits(), 3);
    assert_eq!(device.bytes.lock()[2 * TEST_BLOCK_SIZE], 0b0111_1111);
}

#[def_test]
fn block_allocator_skips_bitmap_checksum_without_metadata_csum() {
    let (filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let journal = JournalTransactions::new(TransactionId::new(329));
    let mut handle = journal.begin(JournalCredits::new(3)).unwrap();

    let allocation = filesystem
        .allocate_block_in_group(BlockGroupNumber::new(0), None, &mut handle)
        .unwrap();

    assert_eq!(allocation.block(), PhysicalBlock::new(6));
}

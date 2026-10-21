// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Component invariants exercised in the kernel with synthetic metadata.

use alloc::{sync::Arc, vec, vec::Vec};

use unittest::def_test;

use crate::{
    BlockGroupNumber, CorruptKind, Ext4Error, FilesystemBlock, InodeKind, InodeNumber,
    inode::InodeInitialization,
    jbd2::{JournalCredits, JournalTransactions, TransactionId},
    superblock::test_fixtures::{
        AllocatorGroupSpec, TEST_BLOCK_COUNT, TEST_BLOCK_SIZE, TEST_EXT4_BG_BLOCK_UNINIT,
        TEST_EXT4_BG_INODE_UNINIT, TEST_FREE_BLOCKS, TEST_FREE_INODES,
        allocator_multigroup_test_filesystem, allocator_test_filesystem,
        allocator_test_filesystem_with_flags, allocator_test_filesystem_with_options,
        enable_allocator_flex_bg, le_u16, le_u32,
    },
    sync::Mutex,
};

#[def_test]
fn concurrent_allocator_round_trip_keeps_accounting_invariant() {
    const THREADS: usize = 2;
    const ROUNDS: usize = 6;
    let (filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0x3f);
    let filesystem = Arc::new(filesystem);
    // Writers of one filesystem share a transaction coordinator. Separate
    // journals can reuse transaction IDs and are not a valid concurrency model.
    let journal = Arc::new(JournalTransactions::new(TransactionId::new(1)));
    let allocated = Arc::new(Mutex::new(Vec::new()));
    let mut threads = Vec::new();
    for _ in 0..THREADS {
        let filesystem = filesystem.clone();
        let journal = journal.clone();
        let allocated = allocated.clone();
        threads.push(ktask::spawn(move || {
            for _ in 0..ROUNDS {
                let mut handle = journal.begin(JournalCredits::new(8)).unwrap();
                let inode = filesystem
                    .allocate_inode(
                        None,
                        InodeInitialization::regular_file(0o644, 0, 0),
                        &mut handle,
                    )
                    .unwrap();
                allocated.lock().push(inode.inode());
            }
        }));
    }
    for thread in threads {
        thread.join();
    }
    let mut inodes = allocated.lock().clone();
    assert_eq!(inodes.len(), THREADS * ROUNDS);
    inodes.sort_unstable_by_key(|inode| inode.get());
    inodes.dedup();
    // These inodes are simultaneously live; uniqueness is meaningful here.
    assert_eq!(inodes.len(), THREADS * ROUNDS);
    assert_eq!(
        filesystem.free_inodes_count(),
        TEST_FREE_INODES - inodes.len() as u32
    );
    let mut handle = journal.begin(JournalCredits::new(8)).unwrap();
    let transaction = handle.id();
    for inode in inodes {
        filesystem
            .release_allocated_inode(inode, InodeKind::RegularFile, &mut handle)
            .unwrap();
    }
    drop(handle);
    let commit = journal.force_commit(transaction).unwrap();
    filesystem
        .metadata_io
        .checkpoint_committed(&commit)
        .unwrap();
    journal.finish_checkpoint_for_test(&commit).unwrap();
    assert_eq!(filesystem.free_inodes_count(), TEST_FREE_INODES);
}

#[def_test]
fn inode_allocator_uses_parent_group_for_regular_inode_and_spreads_directories() {
    let (filesystem, _device) = allocator_multigroup_test_filesystem(&[
        AllocatorGroupSpec {
            free_blocks: TEST_FREE_BLOCKS,
            free_inodes: TEST_FREE_INODES,
            used_directories: 7,
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

    let journal = JournalTransactions::new(TransactionId::new(821));
    let mut regular_handle = journal.begin(JournalCredits::new(4)).unwrap();
    let regular = filesystem
        .allocate_inode(
            Some(InodeNumber::new(40)),
            InodeInitialization::regular_file(0o644, 0, 0),
            &mut regular_handle,
        )
        .unwrap();
    assert_eq!(regular.group(), BlockGroupNumber::new(1));
    assert_eq!(regular.inode(), InodeNumber::new(33));
    drop(regular_handle);

    let commit = journal.force_commit(TransactionId::new(821)).unwrap();
    filesystem
        .metadata_io
        .checkpoint_committed(&commit)
        .unwrap();
    journal.finish_checkpoint_for_test(&commit).unwrap();

    let mut directory_handle = journal.begin(JournalCredits::new(4)).unwrap();
    let directory = filesystem
        .allocate_inode(
            None,
            InodeInitialization::directory(0o755, 0, 0),
            &mut directory_handle,
        )
        .unwrap();
    assert_eq!(directory.group(), BlockGroupNumber::new(1));
    assert_eq!(filesystem.groups()[1].used_directories_count(), 1);
}

#[def_test]
fn orlov_top_level_directories_spread_across_groups() {
    let mut groups = [AllocatorGroupSpec {
        free_blocks: TEST_FREE_BLOCKS,
        free_inodes: TEST_FREE_INODES,
        used_directories: 0,
        flags: 0,
        block_bitmap: [0b0011_1111, 0, 0, 0],
        inode_bitmap: [0, 0, 0, 0],
    }; 4];
    groups[0].inode_bitmap = [0xff, 0x03, 0, 0];
    let (filesystem, _device) = allocator_multigroup_test_filesystem(&groups);
    let journal = JournalTransactions::new(TransactionId::new(822));
    let mut handle = journal.begin(JournalCredits::new(32)).unwrap();
    let mut allocated_groups = Vec::new();

    for _ in 0..4 {
        let allocation = filesystem
            .allocate_inode(
                None,
                InodeInitialization::directory(0o755, 0, 0),
                &mut handle,
            )
            .unwrap();
        allocated_groups.push(allocation.group().get());
    }

    allocated_groups.sort_unstable();
    allocated_groups.dedup();
    assert_eq!(allocated_groups, vec![0, 1, 2, 3]);
}

#[def_test]
fn orlov_directory_group_requires_free_blocks_inside_flex() {
    let mut groups = [AllocatorGroupSpec {
        free_blocks: 0,
        free_inodes: TEST_FREE_INODES,
        used_directories: 0,
        flags: 0,
        block_bitmap: [0xff; 4],
        inode_bitmap: [0, 0, 0, 0],
    }; 2];
    groups[0].inode_bitmap = [0xff, 0x03, 0, 0];
    groups[1] = AllocatorGroupSpec {
        free_blocks: TEST_FREE_BLOCKS,
        free_inodes: TEST_FREE_INODES,
        used_directories: 7,
        flags: 0,
        block_bitmap: [0b0011_1111, 0, 0, 0],
        inode_bitmap: [0, 0, 0, 0],
    };
    let (mut filesystem, _device) = allocator_multigroup_test_filesystem(&groups);
    enable_allocator_flex_bg(&mut filesystem, 1);
    let journal = JournalTransactions::new(TransactionId::new(825));
    let mut handle = journal.begin(JournalCredits::new(8)).unwrap();

    let allocation = filesystem
        .allocate_inode(
            None,
            InodeInitialization::directory(0o755, 0, 0),
            &mut handle,
        )
        .unwrap();

    assert_eq!(allocation.group(), BlockGroupNumber::new(1));
}

#[def_test]
fn regular_inode_allocator_uses_quadratic_probe_before_linear_fallback() {
    let mut groups = [AllocatorGroupSpec {
        free_blocks: 0,
        free_inodes: 0,
        used_directories: 0,
        flags: 0,
        block_bitmap: [0xff; 4],
        inode_bitmap: [0xff; 4],
    }; 8];
    groups[0] = AllocatorGroupSpec {
        free_blocks: 0,
        free_inodes: TEST_FREE_INODES,
        used_directories: 0,
        flags: 0,
        block_bitmap: [0xff; 4],
        inode_bitmap: [0xff, 0x03, 0, 0],
    };
    groups[3] = AllocatorGroupSpec {
        free_blocks: TEST_FREE_BLOCKS,
        free_inodes: TEST_FREE_INODES,
        used_directories: 0,
        flags: 0,
        block_bitmap: [0b0011_1111, 0, 0, 0],
        inode_bitmap: [0, 0, 0, 0],
    };
    let (filesystem, _device) = allocator_multigroup_test_filesystem(&groups);
    let journal = JournalTransactions::new(TransactionId::new(823));
    let mut handle = journal.begin(JournalCredits::new(8)).unwrap();

    let allocation = filesystem
        .allocate_inode(
            Some(InodeNumber::new(11)),
            InodeInitialization::regular_file(0o644, 0, 0),
            &mut handle,
        )
        .unwrap();

    assert_eq!(allocation.group(), BlockGroupNumber::new(3));
}

#[def_test]
fn regular_inode_allocator_keeps_parent_flex_group_locality() {
    let mut groups = [AllocatorGroupSpec {
        free_blocks: TEST_FREE_BLOCKS,
        free_inodes: TEST_FREE_INODES,
        used_directories: 0,
        flags: 0,
        block_bitmap: [0b0011_1111, 0, 0, 0],
        inode_bitmap: [0, 0, 0, 0],
    }; 4];
    groups[0].inode_bitmap = [0xff, 0x03, 0, 0];
    groups[2] = AllocatorGroupSpec {
        free_blocks: 0,
        free_inodes: TEST_FREE_INODES,
        used_directories: 0,
        flags: 0,
        block_bitmap: [0xff; 4],
        inode_bitmap: [0, 0, 0, 0],
    };
    let (mut filesystem, _device) = allocator_multigroup_test_filesystem(&groups);
    enable_allocator_flex_bg(&mut filesystem, 1);
    let journal = JournalTransactions::new(TransactionId::new(824));
    let mut handle = journal.begin(JournalCredits::new(8)).unwrap();

    let allocation = filesystem
        .allocate_inode(
            Some(InodeNumber::new(65)),
            InodeInitialization::regular_file(0o644, 0, 0),
            &mut handle,
        )
        .unwrap();

    assert_eq!(allocation.group(), BlockGroupNumber::new(3));
}

#[def_test]
fn inode_allocator_rejects_insufficient_credits_before_metadata_access() {
    let (filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let journal = JournalTransactions::new(TransactionId::new(312));
    let mut handle = journal.begin(JournalCredits::new(3)).unwrap();

    assert_eq!(
        filesystem.allocate_inode(
            None,
            InodeInitialization::regular_file(0o644, 0, 0),
            &mut handle
        ),
        Err(Ext4Error::InsufficientJournalCredits)
    );
    assert_eq!(handle.remaining_credits(), 3);
    assert_eq!(filesystem.groups()[0].free_inodes_count(), TEST_FREE_INODES);
    assert_eq!(filesystem.free_inodes_count(), TEST_FREE_INODES);
}

#[def_test]
fn inode_allocator_rejects_uninit_group_zero() {
    let (filesystem, _device) = allocator_test_filesystem_with_flags(
        TEST_FREE_BLOCKS,
        0b0011_1111,
        TEST_FREE_INODES,
        0,
        0,
        TEST_EXT4_BG_INODE_UNINIT,
    );
    let journal = JournalTransactions::new(TransactionId::new(322));
    let mut handle = journal.begin(JournalCredits::new(4)).unwrap();

    assert_eq!(
        filesystem.allocate_inode_in_group(
            BlockGroupNumber::new(0),
            InodeInitialization::regular_file(0o644, 0, 0),
            &mut handle,
        ),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeBitmap))
    );
    assert_eq!(handle.remaining_credits(), 4);
}

#[def_test]
fn inode_allocator_rejects_initialized_bitmap_checksum_mismatch() {
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
        false,
        true,
    );
    let journal = JournalTransactions::new(TransactionId::new(330));
    let mut handle = journal.begin(JournalCredits::new(4)).unwrap();

    assert_eq!(
        filesystem.allocate_inode_in_group(
            BlockGroupNumber::new(0),
            InodeInitialization::regular_file(0o644, 0, 0),
            &mut handle,
        ),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeBitmap))
    );
    assert_eq!(handle.remaining_credits(), 4);
    assert_eq!(device.bytes.lock()[3 * TEST_BLOCK_SIZE + 1], 0x03);
}

#[def_test]
fn inode_allocator_release_rejects_initialized_bitmap_checksum_mismatch() {
    let (filesystem, device) = allocator_test_filesystem_with_options(
        AllocatorGroupSpec {
            free_blocks: TEST_FREE_BLOCKS,
            free_inodes: TEST_FREE_INODES - 1,
            used_directories: 0,
            flags: 0,
            block_bitmap: [0b0011_1111, 0, 0, 0],
            inode_bitmap: [0xff, 0x07, 0, 0],
        },
        true,
        false,
        true,
    );
    let journal = JournalTransactions::new(TransactionId::new(331));
    let mut handle = journal.begin(JournalCredits::new(4)).unwrap();

    assert_eq!(
        filesystem.release_allocated_inode(
            InodeNumber::new(11),
            InodeKind::RegularFile,
            &mut handle
        ),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeBitmap))
    );
    assert_eq!(handle.remaining_credits(), 4);
    assert_eq!(device.bytes.lock()[3 * TEST_BLOCK_SIZE + 1], 0x07);
}

#[def_test]
fn inode_allocator_skips_bitmap_checksum_without_metadata_csum() {
    let (filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let journal = JournalTransactions::new(TransactionId::new(332));
    let mut handle = journal.begin(JournalCredits::new(4)).unwrap();

    let allocation = filesystem
        .allocate_inode_in_group(
            BlockGroupNumber::new(0),
            InodeInitialization::regular_file(0o644, 0, 0),
            &mut handle,
        )
        .unwrap();

    assert_eq!(allocation.inode(), InodeNumber::new(11));
}

#[def_test]
fn inode_allocator_lazy_initializes_same_group_block_bitmap() {
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
            free_inodes: 32,
            used_directories: 0,
            flags: TEST_EXT4_BG_INODE_UNINIT | TEST_EXT4_BG_BLOCK_UNINIT,
            block_bitmap: [0; 4],
            inode_bitmap: [0; 4],
        },
    ]);
    let journal = JournalTransactions::new(TransactionId::new(323));
    let mut handle = journal.begin(JournalCredits::new(5)).unwrap();
    let transaction = handle.id();

    let allocation = filesystem
        .allocate_inode(
            Some(InodeNumber::new(33)),
            InodeInitialization::regular_file(0o644, 0, 0),
            &mut handle,
        )
        .unwrap();

    assert_eq!(allocation.group(), BlockGroupNumber::new(1));
    assert_eq!(allocation.inode(), InodeNumber::new(33));
    assert_eq!(filesystem.groups()[1].free_blocks_count(), TEST_FREE_BLOCKS);
    assert_eq!(filesystem.groups()[1].free_inodes_count(), 31);
    assert_eq!(filesystem.groups()[1].flags(), 0);
    assert_eq!(filesystem.free_blocks_count(), 26);
    assert_eq!(filesystem.free_inodes_count(), 31);
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
    let group1_inode_bitmap_offset = (TEST_BLOCK_COUNT + 3) * TEST_BLOCK_SIZE;
    assert_eq!(
        le_u16(&bytes, group1_descriptor_offset + 18)
            & (TEST_EXT4_BG_INODE_UNINIT | TEST_EXT4_BG_BLOCK_UNINIT),
        0
    );
    assert_eq!(le_u16(&bytes, group1_descriptor_offset + 12), 26);
    assert_eq!(bytes[group1_block_bitmap_offset], 0b0011_1111);
    assert_eq!(bytes[group1_inode_bitmap_offset], 0b0000_0001);
}

#[def_test]
fn inode_allocator_rejects_initialized_bitmap_with_cleared_tail_bit() {
    let (filesystem, device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    device.bytes.lock()[3 * TEST_BLOCK_SIZE + 4] = 0;
    let journal = JournalTransactions::new(TransactionId::new(327));
    let mut handle = journal.begin(JournalCredits::new(4)).unwrap();

    assert_eq!(
        filesystem.allocate_inode_in_group(
            BlockGroupNumber::new(0),
            InodeInitialization::regular_file(0o644, 0, 0),
            &mut handle,
        ),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeBitmap))
    );
    assert_eq!(
        filesystem.allocate_inode(
            None,
            InodeInitialization::regular_file(0o644, 0, 0),
            &mut handle
        ),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeBitmap))
    );
    assert_eq!(handle.remaining_credits(), 4);
    assert_eq!(device.bytes.lock()[3 * TEST_BLOCK_SIZE + 4], 0);
}

#[def_test]
fn inode_allocator_skips_group_with_cleared_reserved_inode_bit() {
    let (filesystem, _device) = allocator_multigroup_test_filesystem(&[
        AllocatorGroupSpec {
            free_blocks: TEST_FREE_BLOCKS,
            free_inodes: TEST_FREE_INODES,
            used_directories: 0,
            flags: 0,
            block_bitmap: [0b0011_1111, 0, 0, 0],
            inode_bitmap: [0xfe, 0x03, 0, 0],
        },
        AllocatorGroupSpec {
            free_blocks: TEST_FREE_BLOCKS,
            free_inodes: 32,
            used_directories: 0,
            flags: 0,
            block_bitmap: [0b0011_1111, 0, 0, 0],
            inode_bitmap: [0, 0, 0, 0],
        },
    ]);
    let journal = JournalTransactions::new(TransactionId::new(328));
    let mut handle = journal.begin(JournalCredits::new(4)).unwrap();

    let allocation = filesystem
        .allocate_inode(
            None,
            InodeInitialization::regular_file(0o644, 0, 0),
            &mut handle,
        )
        .unwrap();

    assert_eq!(allocation.group(), BlockGroupNumber::new(1));
    assert_eq!(allocation.inode(), InodeNumber::new(33));
    assert_eq!(filesystem.groups()[0].free_inodes_count(), TEST_FREE_INODES);
    assert_eq!(filesystem.groups()[1].free_inodes_count(), 31);
}

#[def_test]
fn inode_allocator_journals_bitmap_group_and_superblock_updates() {
    let (filesystem, device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let journal = JournalTransactions::new(TransactionId::new(401));
    let mut handle = journal.begin(JournalCredits::new(4)).unwrap();
    let transaction = handle.id();

    let allocation = filesystem
        .allocate_inode_in_group(
            BlockGroupNumber::new(0),
            InodeInitialization::regular_file(0o644, 0, 0)
                .with_owner(1000, 1001)
                .with_timestamp(crate::Ext4Timestamp::new(123, 0))
                .with_generation(77),
            &mut handle,
        )
        .unwrap();

    assert_eq!(allocation.group(), BlockGroupNumber::new(0));
    assert_eq!(allocation.inode(), InodeNumber::new(11));
    assert_eq!(allocation.bitmap_bit(), 10);
    assert_eq!(filesystem.groups()[0].free_inodes_count(), 21);
    assert_eq!(filesystem.free_inodes_count(), 21);
    let inode = filesystem.internal_iget(allocation.inode()).unwrap();
    assert_eq!(inode.kind(), InodeKind::RegularFile);
    assert_eq!(inode.mode(), 0o100644);
    assert_eq!(inode.uid(), 1000);
    assert_eq!(inode.gid(), 1001);
    assert_eq!(inode.links_count(), 1);
    assert_eq!(inode.generation(), 77);
    assert_eq!(inode.mtime(), crate::Ext4Timestamp::new(123, 0));
    drop(handle);

    let commit = journal.force_commit(transaction).unwrap();
    assert_eq!(commit.used_credits().unwrap(), 4);
    assert_eq!(
        commit.metadata_blocks().unwrap().as_ref(),
        &[
            FilesystemBlock::new(0),
            FilesystemBlock::new(1),
            FilesystemBlock::new(3),
            FilesystemBlock::new(4),
        ]
    );
    filesystem
        .metadata_io
        .checkpoint_committed(&commit)
        .unwrap();
    journal.finish_checkpoint_for_test(&commit).unwrap();

    let bytes = device.bytes();
    assert_eq!(bytes[3 * TEST_BLOCK_SIZE + 1], 0x07);
    assert_eq!(le_u16(&bytes, TEST_BLOCK_SIZE + 14), 21);
    assert_eq!(le_u32(&bytes, 1024 + 0x10), 21);
    let inode_offset = 4 * TEST_BLOCK_SIZE + 10 * 256;
    assert_eq!(le_u16(&bytes, inode_offset), 0o100644);
    assert_eq!(
        le_u32(&bytes, inode_offset + 0x20),
        crate::disk::inode::EXT4_EXTENTS_FL
    );
    assert_eq!(
        le_u16(&bytes, inode_offset + 0x28),
        crate::disk::extent::EXTENT_MAGIC
    );
}

#[def_test]
fn inode_allocator_updates_directory_count_and_release_reverses_it() {
    let (filesystem, device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let journal = JournalTransactions::new(TransactionId::new(501));
    let mut handle = journal.begin(JournalCredits::new(4)).unwrap();
    let allocate_transaction = handle.id();

    let allocation = filesystem
        .allocate_inode_in_group(
            BlockGroupNumber::new(0),
            InodeInitialization::directory(0o755, 0, 0),
            &mut handle,
        )
        .unwrap();
    assert_eq!(filesystem.groups()[0].used_directories_count(), 1);
    assert_eq!(
        filesystem.internal_iget(allocation.inode()).unwrap().kind(),
        InodeKind::Directory
    );
    drop(handle);

    let commit = journal.force_commit(allocate_transaction).unwrap();
    filesystem
        .metadata_io
        .checkpoint_committed(&commit)
        .unwrap();
    journal.finish_checkpoint_for_test(&commit).unwrap();

    let mut handle = journal.begin(JournalCredits::new(4)).unwrap();
    let release_transaction = handle.id();
    let released = filesystem
        .release_allocated_inode(allocation.inode(), InodeKind::Directory, &mut handle)
        .unwrap();

    assert_eq!(released.inode(), InodeNumber::new(11));
    assert_eq!(filesystem.groups()[0].free_inodes_count(), TEST_FREE_INODES);
    assert_eq!(filesystem.groups()[0].used_directories_count(), 0);
    assert_eq!(filesystem.free_inodes_count(), TEST_FREE_INODES);
    drop(handle);

    let commit = journal.force_commit(release_transaction).unwrap();
    filesystem
        .metadata_io
        .checkpoint_committed(&commit)
        .unwrap();
    journal.finish_checkpoint_for_test(&commit).unwrap();

    let bytes = device.bytes();
    assert_eq!(bytes[3 * TEST_BLOCK_SIZE + 1], 0x03);
    assert_eq!(
        le_u16(&bytes, TEST_BLOCK_SIZE + 14),
        TEST_FREE_INODES as u16
    );
    assert_eq!(le_u16(&bytes, TEST_BLOCK_SIZE + 16), 0);
    assert_eq!(le_u32(&bytes, 1024 + 0x10), TEST_FREE_INODES);
    let inode_offset = 4 * TEST_BLOCK_SIZE + 10 * 256;
    assert!(
        bytes[inode_offset..inode_offset + 256]
            .iter()
            .all(|byte| *byte == 0)
    );
}

#[def_test]
fn inode_allocator_rejects_releasing_reserved_inode_without_consuming_credits() {
    let (filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let journal = JournalTransactions::new(TransactionId::new(601));
    let mut handle = journal.begin(JournalCredits::new(4)).unwrap();

    assert_eq!(
        filesystem.release_allocated_inode(
            InodeNumber::new(2),
            InodeKind::RegularFile,
            &mut handle
        ),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeBitmap))
    );
    assert_eq!(handle.remaining_credits(), 4);
}

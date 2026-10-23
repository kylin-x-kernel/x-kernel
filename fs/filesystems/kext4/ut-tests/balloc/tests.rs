// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;
use crate::{
    InodeNumber,
    inode::{InodeInitialization, InodeKind},
    jbd2::{JournalCredits, JournalTransactions, TransactionId},
    superblock::test_fixtures::*,
};

#[def_test]
fn allocation_goals_in_second_group_round_trip_accounting() {
    let (filesystem, device) = allocator_multigroup_test_filesystem(&[
        AllocatorGroupSpec {
            free_blocks: 26,
            free_inodes: 22,
            used_directories: 0,
            flags: 0,
            block_bitmap: [0x3f, 0, 0, 0],
            inode_bitmap: [0xff, 3, 0, 0],
        },
        AllocatorGroupSpec {
            free_blocks: 26,
            free_inodes: 32,
            used_directories: 0,
            flags: 0,
            block_bitmap: [0x3f, 0, 0, 0],
            inode_bitmap: [0, 0, 0, 0],
        },
    ]);
    let free_blocks = filesystem.free_blocks_count();
    let free_inodes = filesystem.free_inodes_count();
    let journal = JournalTransactions::new_with_credit_limit(TransactionId::new(701), 32);
    let mut handle = journal.begin(JournalCredits::new(14)).unwrap();
    let transaction = handle.id();
    let block = filesystem
        .allocate_block(Some(FilesystemBlock::new(42)), &mut handle)
        .unwrap();
    assert!((32..64).contains(&block.block().get()));
    filesystem
        .release_allocated_block(block.block(), &mut handle)
        .unwrap();
    let inode = filesystem
        .allocate_inode(
            Some(InodeNumber::new(33)),
            InodeInitialization::regular_file(0o644, 0, 0),
            &mut handle,
        )
        .unwrap();
    assert!((33..=64).contains(&inode.inode().get()));
    filesystem
        .release_allocated_inode(inode.inode(), InodeKind::RegularFile, &mut handle)
        .unwrap();
    drop(handle);
    let commit = journal.force_commit(transaction).unwrap();
    filesystem
        .metadata_io
        .checkpoint_committed(&commit)
        .unwrap();
    assert_eq!(filesystem.free_blocks_count(), free_blocks);
    assert_eq!(filesystem.free_inodes_count(), free_inodes);
    let reopened = Ext4SbInfo::open(device, false).unwrap();
    assert_eq!(reopened.free_blocks_count(), free_blocks);
    assert_eq!(reopened.free_inodes_count(), free_inodes);
    assert!(!reopened.is_inode_allocated(inode.inode()).unwrap());
}

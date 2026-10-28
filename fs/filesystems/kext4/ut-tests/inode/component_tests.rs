// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Component invariants exercised in the kernel with synthetic metadata.

use unittest::def_test;

use crate::{
    inode::InodeInitialization,
    jbd2::{JournalCredits, JournalTransactions, TransactionId},
    superblock::test_fixtures::{
        TEST_FREE_BLOCKS, allocate_checkpointed_inode, allocate_checkpointed_regular_inode,
        allocator_test_filesystem,
    },
};

#[def_test]
fn inode_entry_publication_preserves_neighbors_from_stale_table_snapshots() {
    let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let first = allocate_checkpointed_regular_inode(&mut filesystem);
    let second = allocate_checkpointed_regular_inode(&mut filesystem);
    let first_location = filesystem.inode_location(first.number()).unwrap();
    let second_location = filesystem.inode_location(second.number()).unwrap();
    assert_eq!(first_location.block, second_location.block);
    // Both operations prepare their entries before either publication, as can
    // happen for independent inodes occupying the same physical table block.
    let mut first_bytes = filesystem
        .read_metadata_block(first_location.block)
        .unwrap()
        .as_ref()
        .to_vec();
    let mut second_bytes = first_bytes.clone();
    filesystem
        .update_referenced_inode_table_entry(&mut first_bytes, &first, |bytes| {
            super::put_u32(bytes, 0x04, 123)
        })
        .unwrap();
    filesystem
        .update_referenced_inode_table_entry(&mut second_bytes, &second, |bytes| {
            super::put_u32(bytes, 0x04, 456)
        })
        .unwrap();
    let journal = JournalTransactions::new(TransactionId::new(1691));
    let mut handle = journal.begin(JournalCredits::new(1)).unwrap();
    let access = filesystem
        .metadata_io
        .write_access(first_location.block, &mut handle)
        .unwrap();
    filesystem
        .replace_inode_table_entry(&access, first.number(), &first_bytes)
        .unwrap();
    filesystem
        .replace_inode_table_entry(&access, second.number(), &second_bytes)
        .unwrap();
    assert_eq!(filesystem.raw_inode(first.number()).unwrap().size(), 123);
    assert_eq!(filesystem.raw_inode(second.number()).unwrap().size(), 456);
}

#[def_test]
fn newly_allocated_inode_starts_at_effective_want_extra_isize() {
    let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let effective_want_extra_isize = filesystem.superblock().want_extra_isize();
    assert_eq!(effective_want_extra_isize, 32);

    let inode = allocate_checkpointed_regular_inode(&mut filesystem);

    assert_eq!(inode.extra_isize(), effective_want_extra_isize);
    assert_eq!(
        filesystem.raw_inode(inode.number()).unwrap().extra_isize(),
        effective_want_extra_isize
    );
}

#[def_test]
fn newly_allocated_inode_preserves_extended_timestamp() {
    let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let timestamp = crate::Ext4Timestamp::new(2_147_483_648, 123_456_789);

    let inode = allocate_checkpointed_inode(
        &mut filesystem,
        InodeInitialization::regular_file(0o644, 0, 0).with_timestamp(timestamp),
    );

    assert_eq!(inode.atime(), timestamp);
    assert_eq!(inode.ctime(), timestamp);
    assert_eq!(inode.mtime(), timestamp);
}

#[def_test]
fn inode_metadata_mutation_updates_the_inode_component_in_place() {
    let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = allocate_checkpointed_regular_inode(&mut filesystem);
    let journal = JournalTransactions::new(TransactionId::new(691));
    let mut handle = journal.begin(JournalCredits::new(4)).unwrap();
    let transaction = handle.id();

    filesystem
        .update_inode_size_metadata(&inode, 123, crate::Ext4Timestamp::new(691, 0), &mut handle)
        .unwrap();

    assert_eq!(inode.size(), 123);
    drop(handle);
    let commit = journal.force_commit(transaction).unwrap();
    filesystem
        .metadata_io
        .checkpoint_committed(&commit)
        .unwrap();
    journal.finish_checkpoint_for_test(&commit).unwrap();
}

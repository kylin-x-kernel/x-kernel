// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Component invariants exercised in the kernel with synthetic metadata.

use alloc::vec;

use block::DriverError;
use unittest::def_test;

use crate::{
    BlockMapping, Ext4Error, Ext4SyncIntent, FilesystemBlock, LogicalBlock, PhysicalBlock,
    jbd2::{JournalCredits, TransactionId},
    superblock::test_fixtures::{
        TEST_BLOCK_SIZE, TEST_FREE_BLOCKS, allocate_checkpointed_regular_inode,
        allocator_test_filesystem, install_credit_limited_journal, install_test_internal_journal,
        journal_allocator_test_filesystem,
    },
};

#[def_test]
fn sync_filesystem_flushes_device_and_reclaims_clean_metadata() {
    let (mut filesystem, device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let metadata = filesystem
        .read_metadata_block(FilesystemBlock::new(2))
        .expect("read metadata through cache");
    drop(metadata);

    filesystem.sync_filesystem().expect("sync filesystem");

    assert_eq!(device.flush_count(), 1);
    assert_eq!(filesystem.metadata_io.reclaim_unused(1), 0);
}

#[def_test]
fn sync_filesystem_propagates_flush_error() {
    let (mut filesystem, device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    device.fail_flush_at(1);

    assert_eq!(
        filesystem.sync_filesystem(),
        Err(Ext4Error::Device(DriverError::Io))
    );
    assert_eq!(device.flush_count(), 1);
}

#[def_test]
fn expected_error_before_metadata_access_does_not_poison_mount_journal() {
    let (mut filesystem, _device) = journal_allocator_test_filesystem(0, 0xff);
    install_test_internal_journal(&mut filesystem, 250);
    let journal = filesystem.metadata_journal().unwrap();
    let mut handle = journal.begin(JournalCredits::new(4)).unwrap();
    let transaction = handle.id();

    assert_eq!(
        filesystem.allocate_block(None, &mut handle),
        Err(Ext4Error::NoSpace)
    );
    assert!(!handle.has_updates());
    assert_eq!(
        filesystem.complete_metadata_mutation(handle, Err::<(), _>(Ext4Error::NoSpace)),
        Err(Ext4Error::NoSpace)
    );
    assert_eq!(filesystem.groups()[0].free_blocks_count(), 0);
    assert!(!journal.is_aborted());
    assert_eq!(journal.running_transaction().unwrap(), Some(transaction));

    let retry = journal.begin(JournalCredits::new(4)).unwrap();
    assert_eq!(retry.id(), transaction);
    drop(retry);
}

#[def_test]
fn expected_error_on_empty_handle_does_not_abort_other_active_handle() {
    let (mut filesystem, _device) =
        journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    install_test_internal_journal(&mut filesystem, 251);
    let journal = filesystem.metadata_journal().unwrap();

    let mut first = journal.begin(JournalCredits::new(4)).unwrap();
    let transaction = first.id();
    let allocation = filesystem.allocate_block(None, &mut first).unwrap();
    let second = journal.begin(JournalCredits::new(1)).unwrap();
    assert_eq!(
        filesystem.complete_metadata_mutation(second, Err::<(), _>(Ext4Error::NoSpace)),
        Err(Ext4Error::NoSpace)
    );
    assert!(!journal.is_aborted());
    assert_eq!(journal.running_transaction().unwrap(), Some(transaction));
    assert_eq!(
        filesystem.groups()[0].free_blocks_count(),
        TEST_FREE_BLOCKS - 1
    );
    assert_eq!(allocation.block(), PhysicalBlock::new(6));

    filesystem
        .complete_metadata_mutation(first, Ok(()))
        .expect("finish surviving metadata mutation");
    assert!(!journal.is_aborted());
}

#[def_test]
fn successful_handle_can_finish_while_another_handle_remains_active() {
    let (mut filesystem, _device) =
        journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    install_credit_limited_journal(&mut filesystem, 252, 341, true);
    let journal = filesystem.metadata_journal().unwrap();

    let mut first = journal.begin(JournalCredits::new(4)).unwrap();
    let transaction = first.id();
    let second = journal.begin(JournalCredits::new(1)).unwrap();
    filesystem.allocate_block(None, &mut first).unwrap();

    filesystem
        .complete_metadata_mutation(first, Ok(()))
        .expect("finish first handle while second remains active");
    assert!(!journal.is_aborted());
    assert_eq!(journal.running_transaction().unwrap(), Some(transaction));

    filesystem
        .complete_metadata_mutation(second, Ok(()))
        .expect("finish last handle below the transaction limit");
    assert!(!journal.is_aborted());
    assert_eq!(journal.running_transaction().unwrap(), Some(transaction));
}

#[def_test]
fn expected_error_after_metadata_access_aborts_without_rewinding_state() {
    let (mut filesystem, _device) =
        journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    install_test_internal_journal(&mut filesystem, 253);
    let journal = filesystem.metadata_journal().unwrap();
    let mut handle = journal.begin(JournalCredits::new(4)).unwrap();

    let allocation = filesystem.allocate_block(None, &mut handle).unwrap();
    assert_eq!(allocation.block(), PhysicalBlock::new(6));
    assert_eq!(
        filesystem.groups()[0].free_blocks_count(),
        TEST_FREE_BLOCKS - 1
    );

    assert_eq!(
        filesystem.complete_metadata_mutation(handle, Err::<(), _>(Ext4Error::NoSpace)),
        Err(Ext4Error::InvalidJournalTransaction)
    );
    assert!(journal.is_aborted());
    assert_eq!(
        filesystem.groups()[0].free_blocks_count(),
        TEST_FREE_BLOCKS - 1
    );
    assert_eq!(
        filesystem.free_blocks_count(),
        u64::from(TEST_FREE_BLOCKS - 1)
    );
    assert!(matches!(
        journal.begin(JournalCredits::new(1)),
        Err(Ext4Error::JournalAborted)
    ));
}

#[def_test]
fn commit_failure_after_metadata_publication_aborts_mount_journal() {
    let (mut filesystem, device) = journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    install_test_internal_journal(&mut filesystem, 254);
    let journal = filesystem.metadata_journal().unwrap();
    let mut handle = journal.begin(JournalCredits::new(4)).unwrap();
    filesystem.allocate_block(None, &mut handle).unwrap();
    device.fail_flush_at(device.flush_count() + 1);

    assert_eq!(
        filesystem.complete_metadata_mutation_with_policy(
            handle,
            Ok(()),
            crate::journal::RecoveryFlagPolicy::PreserveDuringRecovery,
        ),
        Err(Ext4Error::Device(DriverError::Io))
    );
    assert!(journal.is_aborted());
    assert!(matches!(
        journal.begin(JournalCredits::new(1)),
        Err(Ext4Error::JournalAborted)
    ));
}

#[def_test]
fn explicit_sync_commit_failure_aborts_mount_journal() {
    let (mut filesystem, device) = journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    install_credit_limited_journal(&mut filesystem, 255, 341, true);
    let journal = filesystem.metadata_journal().unwrap();
    let mut handle = journal.begin(JournalCredits::new(4)).unwrap();
    filesystem.allocate_block(None, &mut handle).unwrap();
    filesystem
        .complete_metadata_mutation(handle, Ok(()))
        .expect("leave successful metadata in the running transaction");
    device.fail_flush_at(device.flush_count() + 1);

    assert_eq!(
        filesystem.sync_filesystem(),
        Err(Ext4Error::Device(DriverError::Io))
    );
    assert!(journal.is_aborted());
    assert_eq!(filesystem.sync_filesystem(), Err(Ext4Error::JournalAborted));
    assert!(matches!(
        journal.begin(JournalCredits::new(1)),
        Err(Ext4Error::JournalAborted)
    ));
}

#[def_test]
fn failed_handle_reusing_shared_metadata_aborts_mount_journal() {
    let (mut filesystem, _device) =
        journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    // Revoke support allows both handles to share an uncommitted transaction.
    install_credit_limited_journal(&mut filesystem, 256, 341, true);
    let journal = filesystem.metadata_journal().unwrap();

    let mut first = journal.begin(JournalCredits::new(4)).unwrap();
    let transaction = first.id();
    filesystem.allocate_block(None, &mut first).unwrap();
    filesystem
        .complete_metadata_mutation(first, Ok(()))
        .expect("retain the first allocation in the running transaction");

    let mut second = journal.begin(JournalCredits::new(4)).unwrap();
    assert_eq!(second.id(), transaction);
    filesystem.allocate_block(None, &mut second).unwrap();
    assert!(second.has_updates());
    assert_eq!(
        filesystem.complete_metadata_mutation(second, Err::<(), _>(Ext4Error::NoSpace)),
        Err(Ext4Error::InvalidJournalTransaction)
    );
    assert!(journal.is_aborted());
}

#[def_test]
fn ordered_writeback_batches_and_inodes_share_one_data_barrier() {
    let (mut filesystem, device) = journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let first = allocate_checkpointed_regular_inode(&mut filesystem);
    let second = allocate_checkpointed_regular_inode(&mut filesystem);
    install_credit_limited_journal(&mut filesystem, 720, 341, true);
    drop(filesystem.metadata_journal().unwrap());
    let before = device.flush_count();
    let input = vec![0x6a; TEST_BLOCK_SIZE];
    for inode in [&first, &second] {
        inode.set_size((TEST_BLOCK_SIZE * 2) as u64);
        for offset in [0, TEST_BLOCK_SIZE as u64] {
            filesystem
                .writeback_ordered_at_with_prealloc_budget(
                    inode,
                    offset,
                    &input,
                    inode.size(),
                    crate::Ext4Timestamp::new(5690, 0),
                    Ext4SyncIntent::FullMetadata,
                    0,
                )
                .unwrap();
        }
    }
    assert_eq!(device.flush_count(), before);
    let journal = filesystem.journal.as_ref().unwrap().clone();
    let commit = journal
        .transactions
        .force_commit(TransactionId::new(720))
        .unwrap();
    assert!(commit.has_unflushed_ordered_data().unwrap());
    assert_eq!(
        journal.transactions.start_checkpoint_for_test(&commit),
        Err(Ext4Error::InvalidJournalTransaction)
    );
    let persisted = filesystem.persist_metadata_journal_commit(&commit).unwrap();
    // One shared data barrier followed by recovery, activation and journal commit.
    assert_eq!(device.flush_count(), before + 4);
    assert!(!commit.has_unflushed_ordered_data().unwrap());
    journal
        .transactions
        .start_checkpoint(&commit, persisted)
        .unwrap();
}

#[def_test]
fn ordered_data_barrier_precedes_journal_writes_and_failure_aborts() {
    use alloc::sync::Arc;

    use crate::{
        buffer::Ext4MetadataIo, io::FilesystemDevice, superblock::fault_device::BufferedFaultDevice,
    };

    for fail_data_flush in [true, false] {
        let (mut filesystem, original) =
            journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
        let inode = allocate_checkpointed_regular_inode(&mut filesystem);
        install_credit_limited_journal(&mut filesystem, 721, 341, true);
        drop(filesystem.metadata_journal().unwrap());
        let initial = original.bytes();
        // Two file-data writes precede commit. Either fail its first barrier,
        // or fail the first recovery-evidence write after that barrier.
        let device = Arc::new(BufferedFaultDevice::new(
            initial.clone(),
            if fail_data_flush { None } else { Some(3) },
            if fail_data_flush { Some(1) } else { None },
        ));
        filesystem.device = Arc::new(
            FilesystemDevice::open(
                crate::io::test_block_device(device.clone()),
                TEST_BLOCK_SIZE,
                (initial.len() / TEST_BLOCK_SIZE) as u64,
            )
            .unwrap(),
        );
        filesystem.metadata_io = Ext4MetadataIo::new(filesystem.device.clone());
        inode.set_size((TEST_BLOCK_SIZE * 2) as u64);
        let input = vec![0x8c; TEST_BLOCK_SIZE];
        for offset in [0, TEST_BLOCK_SIZE as u64] {
            filesystem
                .writeback_ordered_at_with_prealloc_budget(
                    &inode,
                    offset,
                    &input,
                    inode.size(),
                    crate::Ext4Timestamp::new(5691, 0),
                    Ext4SyncIntent::FullMetadata,
                    0,
                )
                .unwrap();
        }
        assert_eq!(device.committed_bytes(), initial);
        let journal = filesystem.journal.as_ref().unwrap().clone();
        let commit = journal
            .transactions
            .force_commit(TransactionId::new(721))
            .unwrap();
        assert_eq!(
            filesystem.commit_metadata_transaction(TransactionId::new(721)),
            Err(Ext4Error::Device(DriverError::Io))
        );
        assert!(journal.is_aborted());
        assert_eq!(
            filesystem.sync_inode(&inode, Ext4SyncIntent::FullMetadata),
            Err(Ext4Error::JournalAborted)
        );
        assert_eq!(
            commit.has_unflushed_ordered_data().unwrap(),
            fail_data_flush
        );
        let committed = device.committed_bytes();
        if fail_data_flush {
            assert_eq!(committed, initial);
        } else {
            for logical in 0..2 {
                let BlockMapping::Mapped { physical, .. } = filesystem
                    .map_blocks(&inode, LogicalBlock::new(logical))
                    .unwrap()
                else {
                    panic!("completed data mapping");
                };
                let start = physical.get() as usize * TEST_BLOCK_SIZE;
                assert_eq!(&committed[start..start + TEST_BLOCK_SIZE], &input);
            }
        }
    }
}

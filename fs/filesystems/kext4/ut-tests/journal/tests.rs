// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Journal lifecycle tests executed in the kernel with in-memory storage.

use block::DriverError;
use unittest::{assert_eq, assert_ne, def_test};

use super::*;
use crate::{
    Ext4Inode, Ext4SyncIntent,
    inode::InodeInitialization,
    superblock::{JournalStatus, test_fixtures::*},
};

#[def_test]
fn mutation_admission_rejects_nested_handles_before_waiting() {
    let (mut filesystem, device) = recovery_test_filesystem();
    filesystem.metadata_journal().unwrap();
    limit_transaction_credits(&mut filesystem, 4);
    let journal = filesystem
        .metadata_journal_for_mutation(
            JournalCredits::new(3),
            RecoveryFlagPolicy::ClearAfterCheckpoint,
        )
        .unwrap();
    let handle = journal.begin(JournalCredits::new(3)).unwrap();
    let writes = device.write_count();
    assert!(matches!(
        filesystem.metadata_journal_for_mutation(
            JournalCredits::new(2),
            RecoveryFlagPolicy::ClearAfterCheckpoint,
        ),
        Err(Ext4Error::InvalidJournalTransaction)
    ));
    assert_eq!(device.write_count(), writes);
    assert!(!journal.is_aborted());
    handle.stop().unwrap();
    filesystem
        .metadata_journal_for_mutation(
            JournalCredits::new(2),
            RecoveryFlagPolicy::ClearAfterCheckpoint,
        )
        .unwrap();
}

#[def_test]
fn exhausted_handle_aborts_and_rejects_following_mutations() {
    let (mut filesystem, device) = recovery_test_filesystem();
    let journal = filesystem.metadata_journal().unwrap();
    let mut handle = journal.begin(JournalCredits::new(1)).unwrap();
    let block = FilesystemBlock::new(6);
    let access = filesystem
        .metadata_io
        .write_access(block, &mut handle)
        .unwrap();
    drop(access);
    let result = filesystem
        .metadata_io
        .write_access(FilesystemBlock::new(7), &mut handle)
        .map(|_| ());
    assert_eq!(result, Err(Ext4Error::InsufficientJournalCredits));
    assert!(handle.has_updates());
    // Exhaustion after metadata publication violates mutation prevalidation.
    assert_eq!(
        filesystem.complete_metadata_mutation(handle, result),
        Err(Ext4Error::InvalidJournalTransaction)
    );
    assert!(journal.is_aborted());
    let writes = device.write_count();
    assert!(matches!(
        filesystem.metadata_journal(),
        Err(Ext4Error::JournalAborted)
    ));
    assert!(matches!(
        journal.begin(JournalCredits::new(1)),
        Err(Ext4Error::JournalAborted)
    ));
    assert_eq!(device.write_count(), writes);
}

fn revoke_journal_test_filesystem(sequence: u32) -> (Ext4SbInfo, Arc<TestDevice>) {
    revoke_journal_test_filesystem_with_log_blocks(sequence, 1023)
}

fn revoke_journal_test_filesystem_with_log_blocks(
    sequence: u32,
    log_blocks: u32,
) -> (Ext4SbInfo, Arc<TestDevice>) {
    let (mut filesystem, device) = journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let mut bytes = test_journal_superblock_bytes_with_blocks(sequence, 1024);
    // Keep the valid minimum journal size while limiting its usable log ring.
    let first = 1024 - log_blocks;
    bytes[0x14..0x18].copy_from_slice(&first.to_be_bytes());
    bytes[0x58..0x5c].copy_from_slice(&first.to_be_bytes());
    // JBD2 v2 is required for revoke and deferred checkpointing. The shared
    // fixture's default v1 journal deliberately exercises synchronous fallback.
    bytes[4..8].copy_from_slice(&4u32.to_be_bytes());
    assert!(enable_superblock_revoke(&mut bytes).unwrap());
    let superblock = JournalSuperblock::decode(&bytes, 4096, 1024, [0; 16]).unwrap();
    install_test_journal(&mut filesystem, superblock);
    assert!(filesystem.journal_supports_revoke());
    (filesystem, device)
}

fn filled_revoke_journal_test_filesystem(
    sequence: u32,
    log_blocks: u32,
    transactions: usize,
) -> (Ext4SbInfo, Arc<TestDevice>) {
    // Each allocation logs three metadata blocks plus descriptor and commit.
    let (mut filesystem, device) =
        revoke_journal_test_filesystem_with_log_blocks(sequence, log_blocks);
    let journal = filesystem.metadata_journal().unwrap();
    for _ in 0..transactions {
        let mut handle = journal.begin(JournalCredits::new(4)).unwrap();
        let transaction = handle.id();
        filesystem
            .allocate_block(Some(FilesystemBlock::new(6)), &mut handle)
            .unwrap();
        drop(handle);
        filesystem.commit_metadata_transaction(transaction).unwrap();
    }
    (filesystem, device)
}

#[def_test]
fn space_pressure_reclaims_a_transaction_budget_and_preserves_the_suffix() {
    let (mut filesystem, device) = filled_revoke_journal_test_filesystem(550, 37, 7);
    let journal = filesystem.metadata_journal().unwrap();
    let original = journal.transactions.checkpoint_batch().unwrap();
    let free_before = filesystem.free_blocks_count();
    let mut handle = journal.begin(JournalCredits::new(4)).unwrap();
    let transaction = handle.id();
    filesystem
        .allocate_block(Some(FilesystemBlock::new(6)), &mut handle)
        .unwrap();
    drop(handle);
    let flushes_before = device.flush_count();
    filesystem.commit_metadata_transaction(transaction).unwrap();

    // With 12 credits, the next transaction needs up to 14 log blocks. The
    // incoming 5-block append plus that reserve needs four old commits, rather
    // than the single commit that would merely unblock this append, or all seven.
    let pending = journal.transactions.checkpoint_batch().unwrap();
    assert_eq!(pending.len(), 4);
    assert!(Arc::ptr_eq(&pending[0].0, &original[4].0));
    assert_eq!(pending[3].0.id(), transaction);
    assert_eq!(
        journal.superblock().start(),
        crate::jbd2::JournalStart::Block(original[4].1.log().start())
    );
    assert_eq!(journal.superblock().sequence(), original[4].0.id());
    // All four selected commits share home and tail barriers; append adds one.
    assert_eq!(device.flush_count(), flushes_before + 3);
    assert_eq!(filesystem.free_blocks_count(), free_before - 1);
    assert!(filesystem.needs_recovery());
    let scan = crate::jbd2::scan_journal(&journal.superblock(), &filesystem).unwrap();
    assert_eq!(scan.transactions().len(), 4);
    assert_eq!(scan.transactions()[0].sequence(), original[4].0.id());
    let bytes = device.bytes();
    let on_disk =
        Ext4DiskSuperblock::decode(&bytes[1024..1024 + superblock::SUPERBLOCK_SIZE]).unwrap();
    assert!(on_disk.features().needs_recovery());
    // Home metadata is only through the fourth old transaction. Reloading that
    // prefix would overwrite live allocator counters from the later commits.
    assert_eq!(on_disk.on_disk_free_blocks_count(), free_before + 3);
    drop(bytes);

    let mut handle = journal.begin(JournalCredits::new(4)).unwrap();
    let next = handle.id();
    filesystem
        .allocate_block(Some(FilesystemBlock::new(6)), &mut handle)
        .unwrap();
    drop(handle);
    let flushes_before = device.flush_count();
    filesystem.commit_metadata_transaction(next).unwrap();
    assert_eq!(device.flush_count(), flushes_before + 1);
    assert_eq!(pending_checkpoint_count(&filesystem), 5);
    assert_eq!(
        journal.superblock().start(),
        crate::jbd2::JournalStart::Block(original[4].1.log().start())
    );
    assert_eq!(filesystem.free_blocks_count(), free_before - 2);

    filesystem.sync_filesystem().unwrap();
    assert_eq!(pending_checkpoint_count(&filesystem), 0);
    assert!(!journal_status(&filesystem).has_nonzero_log_start());
    assert!(!filesystem.needs_recovery());
    let bytes = device.bytes();
    let on_disk =
        Ext4DiskSuperblock::decode(&bytes[1024..1024 + superblock::SUPERBLOCK_SIZE]).unwrap();
    assert_eq!(on_disk.on_disk_free_blocks_count(), free_before - 2);
}

#[def_test]
fn pressure_prefix_leaves_unrelated_cache_for_cache_reclaim() {
    let (mut filesystem, _) = filled_revoke_journal_test_filesystem(555, 37, 7);
    let journal = filesystem.metadata_journal().unwrap();
    let original = journal.transactions.checkpoint_batch().unwrap();
    // These read-only blocks are outside every allocation transaction's targets.
    drop(
        filesystem
            .read_metadata_block(FilesystemBlock::new(20))
            .unwrap(),
    );
    let pinned = filesystem
        .read_metadata_block(FilesystemBlock::new(21))
        .unwrap();
    let mut handle = journal.begin(JournalCredits::new(4)).unwrap();
    let transaction = handle.id();
    filesystem
        .allocate_block(Some(FilesystemBlock::new(6)), &mut handle)
        .unwrap();
    drop(handle);
    filesystem.commit_metadata_transaction(transaction).unwrap();

    assert_eq!(pending_checkpoint_count(&filesystem), 4);
    assert_eq!(
        journal.superblock().start(),
        crate::jbd2::JournalStart::Block(original[4].1.log().start())
    );
    assert!(filesystem.needs_recovery());
    // Checkpoint only retires its own block targets. Independent cache cleanup
    // still finds the unrelated clean entry and preserves the pinned reader.
    assert_eq!(filesystem.metadata_io.reclaim_unused(usize::MAX), 1);
    drop(pinned);
    assert_eq!(filesystem.metadata_io.reclaim_unused(usize::MAX), 1);
}

#[def_test]
fn failed_pressure_prefix_preserves_selected_work_suffix_and_committing_append() {
    for failure in 0..4 {
        let (mut filesystem, device) = filled_revoke_journal_test_filesystem(560, 37, 7);
        let journal = filesystem.metadata_journal().unwrap();
        let pending = journal.transactions.checkpoint_batch().unwrap();
        let original = journal.superblock();
        let tail_offset = journal.map_block(JournalBlock::new(0)).unwrap().get() as usize
            * filesystem.device.block_size();
        let before = device.bytes();
        let mut handle = journal.begin(JournalCredits::new(4)).unwrap();
        let transaction = handle.id();
        filesystem
            .allocate_block(Some(FilesystemBlock::new(6)), &mut handle)
            .unwrap();
        drop(handle);
        let commit = journal.transactions.force_commit(transaction).unwrap();
        let selected = journal
            .transactions
            .checkpoint_batch_for_append(&original, &commit)
            .unwrap();
        assert_eq!(selected.transactions.len(), 4);
        assert_eq!(selected.next_oldest, Some(pending[4].1));
        let home_blocks: usize = selected
            .transactions
            .iter()
            .map(|(commit, _)| commit.metadata_blocks().unwrap().len())
            .sum();
        match failure {
            0 => device.fail_write_at(device.write_count() + 4),
            1 => device.fail_flush_at(device.flush_count() + 1),
            2 => device.fail_write_at(device.write_count() + home_blocks + 1),
            _ => device.fail_flush_at(device.flush_count() + 2),
        }
        assert_eq!(
            filesystem.commit_metadata_transaction(transaction),
            Err(Ext4Error::Device(DriverError::Io))
        );
        assert_eq!(pending_checkpoint_count(&filesystem), 7);
        assert_eq!(journal.superblock().sequence(), original.sequence());
        assert_eq!(journal.superblock().start(), original.start());
        let retained = journal.transactions.checkpoint_batch().unwrap_err();
        assert_eq!(retained, Ext4Error::JournalAborted);
        assert_eq!(commit.metadata_blocks().unwrap().len(), 3);
        assert!(journal.is_aborted());
        if failure < 3 {
            let after = device.bytes();
            let tail_end = tail_offset + filesystem.device.block_size();
            assert_eq!(
                &after[tail_offset..tail_end],
                &before[tail_offset..tail_end]
            );
        }
        let scan = crate::jbd2::scan_journal(&original, &filesystem).unwrap();
        assert_eq!(scan.transactions().len(), 7);
        assert!(filesystem.needs_recovery());
        // The original evidence stays attached to both selected and unselected
        // transactions; a failed prefix must retire neither part of the FIFO.
        for (transaction, _) in &pending {
            assert_eq!(transaction.metadata_blocks().unwrap().len(), 3);
        }
        assert_eq!(
            journal.transactions.newest_persisted_commit().unwrap(),
            Some(pending[6].1)
        );
        let writes = device.write_count();
        assert_eq!(filesystem.sync_filesystem(), Err(Ext4Error::JournalAborted));
        assert_eq!(device.write_count(), writes);
    }
}

#[def_test]
fn pressure_budget_drains_a_small_ring_and_allows_append() {
    let (mut filesystem, device) = filled_revoke_journal_test_filesystem(506, 17, 3);
    assert_eq!(pending_checkpoint_count(&filesystem), 3);
    let journal = filesystem.metadata_journal().unwrap();
    let free_blocks_before = filesystem.free_blocks_count() + 3;
    let mut handle = journal.begin(JournalCredits::new(4)).unwrap();
    let transaction = handle.id();
    filesystem
        .allocate_block(Some(FilesystemBlock::new(6)), &mut handle)
        .unwrap();
    drop(handle);
    filesystem.commit_metadata_transaction(transaction).unwrap();

    // Five credits reserve seven log blocks, and the append needs five.
    // Retiring two commits leaves only twelve physical blocks free, so the
    // spare-block rule requires all three commits before the wrapped append.
    assert_eq!(pending_checkpoint_count(&filesystem), 1);
    let pending = journal.transactions.checkpoint_batch().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].0.id(), transaction);
    let status = journal_status(&filesystem);
    assert_eq!(status.sequence(), transaction.get());
    assert!(status.has_nonzero_log_start());
    assert_eq!(
        journal.superblock().start(),
        crate::jbd2::JournalStart::Block(pending[0].1.log().start())
    );
    assert!(filesystem.needs_recovery());
    let scan = crate::jbd2::scan_journal(&journal.superblock(), &filesystem).unwrap();
    assert_eq!(scan.transactions().len(), 1);
    assert_eq!(scan.transactions()[0].sequence(), transaction);
    let bytes = device.bytes();
    let on_disk =
        Ext4DiskSuperblock::decode(&bytes[1024..1024 + superblock::SUPERBLOCK_SIZE]).unwrap();
    assert!(on_disk.features().needs_recovery());
    assert_eq!(on_disk.on_disk_free_blocks_count(), free_blocks_before - 3);
    assert_eq!(filesystem.free_blocks_count(), free_blocks_before - 4);

    filesystem.sync_filesystem().unwrap();
    assert_eq!(pending_checkpoint_count(&filesystem), 0);
    assert!(!journal_status(&filesystem).has_nonzero_log_start());
    assert!(!filesystem.needs_recovery());
    let bytes = device.bytes();
    let on_disk =
        Ext4DiskSuperblock::decode(&bytes[1024..1024 + superblock::SUPERBLOCK_SIZE]).unwrap();
    assert!(!on_disk.features().needs_recovery());
    assert_eq!(on_disk.on_disk_free_blocks_count(), free_blocks_before - 4);
    assert_eq!(filesystem.free_blocks_count(), free_blocks_before - 4);
}

pub(super) fn pending_checkpoint_count(filesystem: &Ext4SbInfo) -> usize {
    let journal = filesystem.journal.as_ref().unwrap();
    lock(&journal.state).checkpoint_policies.len()
}

pub(super) fn journal_status(filesystem: &Ext4SbInfo) -> JournalStatus {
    JournalStatus::from(&filesystem.journal.as_ref().unwrap().superblock())
}

#[def_test]
fn sync_filesystem_drains_pending_metadata_checkpoint() {
    let (mut filesystem, device) = revoke_journal_test_filesystem(500);
    let journal = filesystem
        .metadata_journal()
        .expect("open metadata journal");
    let same_journal = filesystem
        .metadata_journal()
        .expect("reopen metadata journal");
    assert!(Arc::ptr_eq(&journal, &same_journal));
    let mut handle = journal.begin(JournalCredits::new(4)).unwrap();
    let transaction = handle.id();
    let block_goal = FilesystemBlock::new(6);

    filesystem
        .allocate_block(Some(block_goal), &mut handle)
        .expect("allocate a metadata block");
    drop(handle);
    filesystem
        .commit_metadata_transaction(transaction)
        .expect("enqueue checkpoint");

    assert_eq!(pending_checkpoint_count(&filesystem), 1);
    let flush_count = device.flush_count();

    filesystem
        .sync_filesystem()
        .expect("sync pending checkpoint");

    assert_eq!(pending_checkpoint_count(&filesystem), 0);
    assert!(device.flush_count() > flush_count);
    let next_handle = journal.begin(JournalCredits::new(1)).unwrap();
    let next_transaction = next_handle.id();
    assert_eq!(next_transaction.get(), transaction.get().wrapping_add(1));
    drop(next_handle);
    assert_eq!(
        journal.transactions.running_transaction().unwrap(),
        Some(next_transaction)
    );
}

#[def_test]
fn successful_operations_share_running_transaction_until_sync() {
    let (mut filesystem, _device) = revoke_journal_test_filesystem(501);
    let credits = JournalCredits::new(4);
    let block_goal = FilesystemBlock::new(6);

    let journal = filesystem
        .metadata_journal_for_mutation(
            credits,
            crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
        )
        .expect("prepare first metadata mutation");
    let mut first_handle = journal.begin(credits).unwrap();
    let transaction = first_handle.id();
    let first_allocation = filesystem
        .allocate_block(Some(block_goal), &mut first_handle)
        .expect("allocate first block");
    filesystem
        .complete_metadata_mutation(first_handle, Ok(()))
        .expect("finish first metadata mutation");
    assert_eq!(pending_checkpoint_count(&filesystem), 0);
    assert_eq!(
        journal.transactions.running_transaction().unwrap(),
        Some(transaction)
    );

    let second_journal = filesystem
        .metadata_journal_for_mutation(
            credits,
            crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
        )
        .expect("prepare second metadata mutation");
    let mut second_handle = second_journal.begin(credits).unwrap();
    assert_eq!(second_handle.id(), transaction);
    let second_allocation = filesystem
        .allocate_block(Some(block_goal), &mut second_handle)
        .expect("allocate second block");
    filesystem
        .complete_metadata_mutation(second_handle, Ok(()))
        .expect("finish second metadata mutation");

    assert_ne!(first_allocation.block(), second_allocation.block());
    assert_eq!(pending_checkpoint_count(&filesystem), 0);
    filesystem
        .sync_filesystem()
        .expect("commit and checkpoint running transaction");
    assert_eq!(journal.transactions.running_transaction().unwrap(), None);
    assert_eq!(pending_checkpoint_count(&filesystem), 0);
    assert!(!journal_status(&filesystem).has_nonzero_log_start());

    let cleanup_credits = JournalCredits::new(8);
    let cleanup_journal = filesystem
        .metadata_journal_for_mutation(
            cleanup_credits,
            crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
        )
        .expect("prepare cleanup metadata mutation");
    let mut cleanup_handle = cleanup_journal.begin(cleanup_credits).unwrap();
    filesystem
        .release_allocated_block(first_allocation.block(), &mut cleanup_handle)
        .expect("release first block");
    filesystem
        .release_allocated_block(second_allocation.block(), &mut cleanup_handle)
        .expect("release second block");
    filesystem
        .complete_metadata_mutation(cleanup_handle, Ok(()))
        .expect("finish cleanup metadata mutation");
    filesystem.sync_filesystem().expect("sync cleanup");
    drop(filesystem);
}

#[def_test]
fn journal_queue_checkpoints_shared_metadata_in_one_batch() {
    let (mut filesystem, device) = revoke_journal_test_filesystem(502);
    let journal = filesystem
        .metadata_journal()
        .expect("open metadata journal");
    let free_blocks_before = filesystem.free_blocks_count();
    let block_goal = FilesystemBlock::new(6);

    let mut first_handle = journal.begin(JournalCredits::new(4)).unwrap();
    let first_transaction = first_handle.id();
    let first_allocation = filesystem
        .allocate_block(Some(block_goal), &mut first_handle)
        .expect("allocate first journaled block");
    drop(first_handle);
    filesystem
        .commit_metadata_transaction(first_transaction)
        .expect("commit first journal transaction");
    assert_eq!(pending_checkpoint_count(&filesystem), 1);

    let second_journal = filesystem
        .metadata_journal()
        .expect("join coordinator with pending checkpoint");
    assert!(Arc::ptr_eq(&journal, &second_journal));
    assert_eq!(pending_checkpoint_count(&filesystem), 1);
    let mut second_handle = second_journal.begin(JournalCredits::new(4)).unwrap();
    let second_transaction = second_handle.id();
    let second_allocation = filesystem
        .allocate_block(Some(block_goal), &mut second_handle)
        .expect("allocate second journaled block");
    drop(second_handle);
    filesystem
        .commit_metadata_transaction(second_transaction)
        .expect("commit second journal transaction");

    assert_ne!(first_allocation.block(), second_allocation.block());
    assert_eq!(pending_checkpoint_count(&filesystem), 2);
    assert_eq!(filesystem.free_blocks_count(), free_blocks_before - 2);
    assert!(filesystem.needs_recovery());
    let status = journal_status(&filesystem);
    assert_eq!(status.sequence(), first_transaction.get());
    assert!(status.has_nonzero_log_start());
    let scan =
        crate::jbd2::scan_journal(&journal.superblock(), &filesystem).expect("scan two commits");
    assert_eq!(scan.transactions().len(), 2);

    let flushes_before = device.flush_count();
    filesystem.run_checkpoint_worker().unwrap();
    // One home-block barrier, one tail barrier, and one ext4 recovery-bit clear.
    assert_eq!(device.flush_count(), flushes_before + 3);
    assert_eq!(pending_checkpoint_count(&filesystem), 0);
    assert!(!filesystem.needs_recovery());
    let status = journal_status(&filesystem);
    assert_eq!(status.sequence(), second_transaction.get().wrapping_add(1));
    assert!(!status.has_nonzero_log_start());
    let on_disk_bytes = device.bytes();
    let on_disk_superblock =
        Ext4DiskSuperblock::decode(&on_disk_bytes[1024..1024 + superblock::SUPERBLOCK_SIZE])
            .unwrap();
    // Both transactions touched allocator metadata, including the same primary
    // superblock. The final frozen version must win, with no lost allocation.
    assert_eq!(
        on_disk_superblock.on_disk_free_blocks_count(),
        free_blocks_before - 2
    );
    assert!(!on_disk_superblock.features().needs_recovery());
    assert_eq!(filesystem.free_blocks_count(), free_blocks_before - 2);

    let mut cleanup_handle = journal.begin(JournalCredits::new(8)).unwrap();
    let cleanup_transaction = cleanup_handle.id();
    filesystem
        .release_allocated_block(first_allocation.block(), &mut cleanup_handle)
        .expect("release first test block");
    filesystem
        .release_allocated_block(second_allocation.block(), &mut cleanup_handle)
        .expect("release second test block");
    drop(cleanup_handle);
    filesystem
        .commit_metadata_transaction(cleanup_transaction)
        .expect("commit cleanup transaction");
    assert_eq!(pending_checkpoint_count(&filesystem), 1);
    filesystem
        .sync_filesystem()
        .expect("checkpoint cleanup transaction");
    assert_eq!(filesystem.free_blocks_count(), free_blocks_before);
    drop(filesystem);
}

#[def_test]
fn checkpoint_batch_preserves_recovery_for_its_last_transaction() {
    let (mut filesystem, device) = revoke_journal_test_filesystem(505);
    let journal = filesystem.metadata_journal().unwrap();
    let free_blocks_before = filesystem.free_blocks_count();

    let mut first_handle = journal.begin(JournalCredits::new(4)).unwrap();
    let first_transaction = first_handle.id();
    filesystem
        .allocate_block(Some(FilesystemBlock::new(6)), &mut first_handle)
        .unwrap();
    drop(first_handle);
    filesystem
        .commit_metadata_transaction(first_transaction)
        .unwrap();
    assert_eq!(pending_checkpoint_count(&filesystem), 1);

    let mut second_handle = journal.begin(JournalCredits::new(4)).unwrap();
    let second_transaction = second_handle.id();
    filesystem
        .allocate_block(Some(FilesystemBlock::new(6)), &mut second_handle)
        .unwrap();
    drop(second_handle);
    // Entering the preserving mutation path commits this running transaction
    // behind the older ClearAfterCheckpoint entry and drains both immediately.
    filesystem
        .metadata_journal_for_mutation(
            JournalCredits::new(4),
            RecoveryFlagPolicy::PreserveDuringRecovery,
        )
        .unwrap();
    assert_eq!(pending_checkpoint_count(&filesystem), 0);
    assert_eq!(journal.transactions.running_transaction().unwrap(), None);
    let status = journal_status(&filesystem);
    assert_eq!(status.sequence(), second_transaction.get().wrapping_add(1));
    assert!(!status.has_nonzero_log_start());
    assert!(filesystem.needs_recovery());
    let on_disk_bytes = device.bytes();
    let on_disk_superblock =
        Ext4DiskSuperblock::decode(&on_disk_bytes[1024..1024 + superblock::SUPERBLOCK_SIZE])
            .unwrap();
    // Reload must retain the newest shared superblock's allocator counters
    // while keeping recovery evidence for the remaining recovery work.
    assert_eq!(
        on_disk_superblock.on_disk_free_blocks_count(),
        free_blocks_before - 2
    );
    assert_eq!(filesystem.free_blocks_count(), free_blocks_before - 2);
    assert!(on_disk_superblock.features().needs_recovery());

    // A later normal transaction belongs to a separate batch and can clear
    // recovery evidence after its own home blocks have become durable.
    let mut next_handle = journal.begin(JournalCredits::new(4)).unwrap();
    let next_transaction = next_handle.id();
    filesystem
        .allocate_block(Some(FilesystemBlock::new(6)), &mut next_handle)
        .unwrap();
    drop(next_handle);
    filesystem
        .commit_metadata_transaction(next_transaction)
        .unwrap();
    assert_eq!(pending_checkpoint_count(&filesystem), 1);
    filesystem.drain_pending_checkpoints().unwrap();
    assert_eq!(pending_checkpoint_count(&filesystem), 0);
    assert!(!filesystem.needs_recovery());
    let on_disk_bytes = device.bytes();
    let on_disk_superblock =
        Ext4DiskSuperblock::decode(&on_disk_bytes[1024..1024 + superblock::SUPERBLOCK_SIZE])
            .unwrap();
    assert!(!on_disk_superblock.features().needs_recovery());
    assert_eq!(
        on_disk_superblock.on_disk_free_blocks_count(),
        free_blocks_before - 3
    );
}

#[def_test]
fn failed_checkpoint_batch_keeps_all_pending_work() {
    // Fail after a written prefix, at the home-block barrier, and at the tail
    // barrier. None may release any transaction or permit another mutation.
    for failure in 0..3 {
        let (mut filesystem, device) = revoke_journal_test_filesystem(503);
        let journal = filesystem.metadata_journal().unwrap();
        for _ in 0..2 {
            let mut handle = journal.begin(JournalCredits::new(4)).unwrap();
            let transaction = handle.id();
            filesystem
                .allocate_block(Some(FilesystemBlock::new(6)), &mut handle)
                .unwrap();
            drop(handle);
            filesystem.commit_metadata_transaction(transaction).unwrap();
        }
        let original = journal.superblock();
        let tail_block = journal.map_block(JournalBlock::new(0)).unwrap();
        let tail_offset = tail_block.get() as usize * filesystem.device.block_size();
        let before = device.bytes();
        let batch = journal.transactions.checkpoint_batch().unwrap();
        let flushes_before = device.flush_count();
        if failure == 0 {
            let first_blocks = batch[0].0.metadata_blocks().unwrap().len();
            device.fail_write_at(device.write_count() + first_blocks + 1);
        } else {
            device.fail_flush_at(flushes_before + failure);
        }
        assert_eq!(
            filesystem.run_checkpoint_worker(),
            Err(Ext4Error::Device(DriverError::Io))
        );
        assert_eq!(device.flush_count(), flushes_before + failure);
        assert_eq!(pending_checkpoint_count(&filesystem), 2);
        let retained = journal
            .transactions
            .newest_persisted_commit()
            .unwrap()
            .unwrap();
        assert_eq!(retained, batch[1].1);
        assert_eq!(journal.superblock().sequence(), original.sequence());
        assert_eq!(journal.superblock().start(), original.start());
        if failure < 2 {
            let after = device.bytes();
            let tail_end = tail_offset + filesystem.device.block_size();
            assert_eq!(
                &after[tail_offset..tail_end],
                &before[tail_offset..tail_end]
            );
        }
        // The old recovery range remains intact even if the tail write reached
        // the device before its flush failed. Do not reclaim or overwrite it.
        let scan = crate::jbd2::scan_journal(&original, &filesystem).unwrap();
        assert_eq!(scan.transactions().len(), 2);
        assert!(journal.is_aborted());
        assert!(matches!(
            journal.begin(JournalCredits::new(1)),
            Err(Ext4Error::JournalAborted)
        ));
        assert_eq!(filesystem.sync_filesystem(), Err(Ext4Error::JournalAborted));
    }
}

fn allocate_regular_inode(filesystem: &mut Ext4SbInfo) -> Ext4Inode {
    let journal = filesystem.metadata_journal().unwrap();
    let mut handle = journal.begin(JournalCredits::new(8)).unwrap();
    let allocation = filesystem
        .allocate_inode(
            None,
            InodeInitialization::regular_file(0o644, 1000, 1000),
            &mut handle,
        )
        .unwrap();
    filesystem
        .complete_metadata_mutation(handle, Ok(()))
        .unwrap();
    filesystem.load_inode_private(allocation.inode()).unwrap()
}

#[def_test]
fn inode_sync_conservatively_commits_the_current_running_transaction() {
    let (mut filesystem, device) = revoke_journal_test_filesystem(504);
    let first = allocate_regular_inode(&mut filesystem);
    let journal = filesystem
        .metadata_journal()
        .expect("open metadata journal");
    let first_transaction = journal
        .transactions
        .running_transaction()
        .unwrap()
        .expect("first transaction remains running");

    let flushes_before_first_sync = device.flush_count();
    filesystem
        .sync_inode(&first, Ext4SyncIntent::FullMetadata)
        .expect("sync first inode transaction");
    // Persist ext4 recovery evidence, activate the clean journal, and
    // finish the log commit. No fourth sync_inode-only flush is needed.
    assert_eq!(device.flush_count(), flushes_before_first_sync + 3);

    assert_eq!(journal.transactions.running_transaction().unwrap(), None);
    assert_eq!(pending_checkpoint_count(&filesystem), 1);

    let flushes_before_data_only_sync = device.flush_count();
    filesystem
        .sync_inode(&first, Ext4SyncIntent::DataOnly)
        .expect("sync inode without a running metadata transaction");
    assert_eq!(device.flush_count(), flushes_before_data_only_sync + 1);

    let _second = allocate_regular_inode(&mut filesystem);
    let second_transaction = journal
        .transactions
        .running_transaction()
        .unwrap()
        .expect("second transaction remains running");
    assert_ne!(second_transaction, first_transaction);

    let flushes_before_second_sync = device.flush_count();
    filesystem
        .sync_inode(&first, Ext4SyncIntent::FullMetadata)
        .expect("conservatively sync current transaction");
    // Allocator accounting no longer replaces the decoded superblock, so
    // the recovery bit persisted by the first commit is still set and the
    // second commit skips the redundant evidence write; only the commit
    // barrier flush remains.
    assert_eq!(device.flush_count(), flushes_before_second_sync + 1);

    assert_eq!(journal.transactions.running_transaction().unwrap(), None);
    assert_eq!(pending_checkpoint_count(&filesystem), 2);

    filesystem.sync_filesystem().expect("sync remaining work");
    assert_eq!(pending_checkpoint_count(&filesystem), 0);
    drop(filesystem);
}

/// Restrict transaction credits without inventing an invalid short journal.
pub(crate) fn limit_transaction_credits(filesystem: &mut Ext4SbInfo, credits: u32) {
    let journal = Arc::get_mut(filesystem.journal.as_mut().unwrap()).unwrap();
    journal.transactions =
        JournalTransactions::new_with_credit_limit(journal.superblock().sequence(), credits);
}

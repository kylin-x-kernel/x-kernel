// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Component invariants with synthetic metadata; no host tools or embedded images.

use alloc::vec::Vec;

use block::DriverError;
use unittest::def_test;

use super::{
    tests::{journal_status, pending_checkpoint_count},
    *,
};
use crate::{
    BlockMapping, DirectoryFileType, InodeKind, LogicalBlock,
    disk::{Ext4DiskSuperblock, features, superblock},
    superblock::test_fixtures::*,
};

#[def_test]
fn recovery_evicts_clean_zero_link_orphan_with_huge_file() {
    let (mut filesystem, device) = recovery_test_filesystem();
    set_allocator_feature_bits(
        &mut filesystem,
        features::CompatFeatures::empty(),
        features::ReadOnlyCompatFeatures::HUGE_FILE,
    );
    let root = filesystem
        .internal_iget(crate::InodeNumber::new(2))
        .expect("read root inode");
    let free_inodes_before_create = filesystem.free_inodes_count();
    let created = filesystem
        .create_regular_file(
            &root,
            b"kext4-recovery-orphan",
            0o644,
            0,
            0,
            crate::Ext4Timestamp::new(130, 0),
        )
        .expect("create orphan recovery target");
    created.set_size(23);
    filesystem
        .writeback_ordered_at(
            &created,
            0,
            b"recover zero-link inode",
            23,
            crate::Ext4Timestamp::new(131, 0),
            crate::Ext4SyncIntent::FullMetadata,
        )
        .expect("write ordinary inode on huge_file filesystem");
    filesystem
        .unlink(
            &root,
            b"kext4-recovery-orphan",
            &created,
            crate::Ext4Timestamp::new(132, 0),
        )
        .expect("leave zero-link inode on the legacy orphan list");
    assert_eq!(created.links_count(), 0);
    assert_eq!(filesystem.orphan_head(), Some(created.number()));
    assert!(filesystem.journal_supports_revoke());
    assert!(!filesystem.needs_recovery());
    filesystem
        .sync_filesystem()
        .expect("persist clean journal and legacy orphan");
    assert_eq!(pending_checkpoint_count(&filesystem), 0);
    assert_eq!(filesystem.orphan_head(), Some(created.number()));
    assert!(!filesystem.needs_recovery());
    drop(filesystem);

    {
        let bytes = device.bytes();
        let persisted =
            Ext4DiskSuperblock::decode(&bytes[1024..1024 + superblock::SUPERBLOCK_SIZE])
                .expect("decode persisted clean orphan superblock");
        assert_eq!(persisted.last_orphan(), created.number().get());
        assert!(!persisted.features().needs_recovery());
    }

    let mount_device: Arc<dyn BlockDeviceOperations> = device.clone();
    assert_eq!(
        open_test_filesystem(mount_device).map(|_| ()),
        Err(Ext4Error::NeedsRecovery)
    );
    let recovery_device: Arc<dyn BlockDeviceOperations> = device.clone();
    assert_eq!(Ext4SbInfo::recover(recovery_device), Ok(None));

    {
        let bytes = device.bytes();
        let persisted =
            Ext4DiskSuperblock::decode(&bytes[1024..1024 + superblock::SUPERBLOCK_SIZE])
                .expect("decode recovered clean orphan superblock");
        assert_eq!(persisted.last_orphan(), 0);
        assert!(!persisted.features().needs_recovery());
    }

    let recovered_device: Arc<dyn BlockDeviceOperations> = device.clone();
    let recovered = open_test_filesystem(recovered_device).expect("mount orphan-cleaned image");
    assert_eq!(recovered.orphan_head(), None);
    assert!(!journal_status(&recovered).has_nonzero_log_start());
    assert_eq!(recovered.free_inodes_count(), free_inodes_before_create);
    assert_eq!(
        recovered
            .lookup(
                &recovered.internal_iget(crate::InodeNumber::new(2)).unwrap(),
                "kext4-recovery-orphan"
            )
            .expect("lookup recovered namespace"),
        None
    );
    drop(recovered);
}

#[def_test]
fn clean_orphan_recovery_flush_failure_preserves_evidence_and_retries() {
    let (mut filesystem, device) = recovery_test_filesystem();
    let root = filesystem
        .internal_iget(crate::InodeNumber::new(2))
        .expect("read root inode");
    let free_inodes_before_create = filesystem.free_inodes_count();
    let created = filesystem
        .create_regular_file(
            &root,
            b"kext4-recovery-flush-failure",
            0o644,
            0,
            0,
            crate::Ext4Timestamp::new(133, 0),
        )
        .expect("create orphan recovery target");
    filesystem
        .unlink(
            &root,
            b"kext4-recovery-flush-failure",
            &created,
            crate::Ext4Timestamp::new(134, 0),
        )
        .expect("leave zero-link inode on the legacy orphan list");
    let orphan = created.number();
    assert_eq!(filesystem.orphan_head(), Some(orphan));
    assert!(filesystem.journal_supports_revoke());
    filesystem
        .sync_filesystem()
        .expect("persist clean journal and legacy orphan");
    assert!(!filesystem.needs_recovery());
    drop(filesystem);

    device.fail_flush_at(device.flush_count() + 1);
    let recovery_device: Arc<dyn BlockDeviceOperations> = device.clone();
    assert_eq!(
        Ext4SbInfo::recover(recovery_device),
        Err(Ext4Error::Device(DriverError::Io))
    );

    {
        let bytes = device.bytes();
        let persisted =
            Ext4DiskSuperblock::decode(&bytes[1024..1024 + superblock::SUPERBLOCK_SIZE])
                .expect("decode recovery-failed superblock");
        assert_eq!(persisted.last_orphan(), orphan.get());
        assert!(persisted.features().needs_recovery());
    }

    let mount_device: Arc<dyn BlockDeviceOperations> = device.clone();
    assert_eq!(
        open_test_filesystem(mount_device).map(|_| ()),
        Err(Ext4Error::NeedsRecovery)
    );

    let retry_device: Arc<dyn BlockDeviceOperations> = device.clone();
    Ext4SbInfo::recover(retry_device).expect("retry clean orphan recovery");

    let recovered_device: Arc<dyn BlockDeviceOperations> = device.clone();
    let recovered = open_test_filesystem(recovered_device).expect("mount retried recovery image");
    assert_eq!(recovered.orphan_head(), None);
    assert!(!recovered.needs_recovery());
    assert!(!journal_status(&recovered).has_nonzero_log_start());
    assert_eq!(recovered.free_inodes_count(), free_inodes_before_create);
    assert_eq!(
        recovered
            .lookup(
                &recovered
                    .internal_iget(crate::InodeNumber::new(2))
                    .expect("read recovered root inode"),
                "kext4-recovery-flush-failure",
            )
            .expect("lookup recovered namespace"),
        None
    );
    drop(recovered);
}

#[def_test]
fn symlink_data_block_recovers_after_journal_replay() {
    // A crash after the block-mapped symlink commit but before the journal
    // checkpoint must leave the image recoverable: replay restores the
    // directory entry, inode, and target data block with the exact target.
    let (filesystem, device) = recovery_test_filesystem();
    drop(filesystem);
    let timestamp = crate::Ext4Timestamp::new(144, 0);
    let target = b"/opt/package/lib/libreplay.so.1.0.0-symlink-journal-replay-target-block";
    {
        let mut filesystem =
            open_test_filesystem(device.clone()).expect("mount journaled namespace image");
        let root = filesystem
            .internal_iget(crate::InodeNumber::new(2))
            .expect("read root inode");
        let symlink = filesystem
            .create_symlink(&root, b"replay-link", target, 0, 0, timestamp)
            .expect("create block-mapped symlink");
        let mut buffer = vec![0u8; target.len()];
        let read = filesystem
            .read_link_at(&symlink, 0, &mut buffer)
            .expect("read symlink target before crash");
        assert_eq!(read, target.len());
        assert_eq!(&buffer[..read], target);
        // create_symlink leaves its small transaction running; commit it
        // so the metadata is persisted to the journal but not checkpointed.
        let committed = filesystem
            .commit_running_metadata_transaction()
            .expect("commit running symlink transaction");
        assert!(committed);
        assert!(pending_checkpoint_count(&filesystem) > 0);
        // Drop without sync_filesystem: the committed metadata lives only
        // in the journal, like a crash before checkpoint.
    }

    assert_eq!(
        open_test_filesystem(device.clone()).map(|_| ()),
        Err(Ext4Error::NeedsRecovery)
    );
    let report = Ext4SbInfo::recover(device.clone())
        .expect("recover journaled symlink commit")
        .expect("expected replayed journal records");
    assert!(report.update_count() > 0);

    let mut filesystem = open_test_filesystem(device.clone()).expect("mount after journal replay");
    let root = filesystem
        .internal_iget(crate::InodeNumber::new(2))
        .expect("read root inode");
    let entry = filesystem
        .lookup(&root, "replay-link")
        .expect("lookup replayed symlink")
        .expect("symlink should survive journal replay");
    assert_eq!(entry.file_type(), DirectoryFileType::Symlink);
    let symlink = filesystem
        .load_inode_private(entry.inode())
        .expect("read replayed symlink inode");
    assert_eq!(symlink.kind(), InodeKind::Symlink);
    let mapping = filesystem
        .map_blocks(&symlink, LogicalBlock::new(0))
        .expect("map replayed symlink block");
    assert!(
        matches!(mapping, BlockMapping::Mapped { .. }),
        "expected mapped symlink target block"
    );
    let mut buffer = vec![0u8; target.len()];
    let read = filesystem
        .read_link_at(&symlink, 0, &mut buffer)
        .expect("read replayed symlink target");
    assert_eq!(read, target.len());
    assert_eq!(&buffer[..read], target);

    filesystem.sync_filesystem().expect("sync replayed image");
    drop(filesystem);
}

#[def_test]
fn recovers_persisted_allocator_journal_commit_before_checkpoint() {
    let (mut filesystem, device) = recovery_test_filesystem();
    let journal = filesystem
        .metadata_journal()
        .expect("journaled test image has an internal journal");
    let mut handle = journal.begin(JournalCredits::new(4)).unwrap();
    let transaction = handle.id();
    let block_goal = FilesystemBlock::new(7);

    let allocation = filesystem
        .allocate_block(Some(block_goal), &mut handle)
        .expect("allocate a block from synthetic journaled filesystem");
    let group_index = usize::try_from(crate::BlockGroupNumber::new(0).get()).unwrap();
    let expected_super_free = filesystem.free_blocks_count();
    let expected_group_free = lock(&filesystem.allocator).groups[group_index].free_blocks_count();
    let bitmap_block = FilesystemBlock::new(
        filesystem
            .group_geometry(crate::BlockGroupNumber::new(0))
            .expect("allocated group has frozen geometry")
            .block_bitmap(),
    );
    let bitmap_bit = allocation.bitmap_bit();
    drop(handle);

    let commit = journal.transactions.force_commit(transaction).unwrap();
    let expected_replay_updates = commit.metadata_blocks().unwrap().as_ref().len();
    filesystem
        .persist_metadata_journal_commit(&commit)
        .expect("persist allocator metadata to the journal");
    drop(filesystem);

    let dirty_device: Arc<dyn BlockDeviceOperations> = device.clone();
    assert_eq!(
        open_test_filesystem(dirty_device).map(|_| ()),
        Err(Ext4Error::NeedsRecovery)
    );
    let recovery_device: Arc<dyn BlockDeviceOperations> = device.clone();
    let report = Ext4SbInfo::recover(recovery_device)
        .expect("recover persisted allocator journal commit")
        .expect("journal recovery was required");
    assert_eq!(report.update_count(), expected_replay_updates);

    let recovered_device: Arc<dyn BlockDeviceOperations> = device.clone();
    let recovered =
        open_test_filesystem(recovered_device).expect("mount recovered allocator image");
    assert!(!recovered.needs_recovery());
    assert_eq!(recovered.free_blocks_count(), expected_super_free);
    assert_eq!(
        lock(&recovered.allocator).groups[group_index].free_blocks_count(),
        expected_group_free
    );
    let bitmap = recovered.read_metadata_block(bitmap_block).unwrap();
    let bitmap_byte = usize::try_from(bitmap_bit / 8).unwrap();
    let bitmap_mask = 1u8 << (bitmap_bit % 8);
    assert_ne!(bitmap.as_ref()[bitmap_byte] & bitmap_mask, 0);
    drop(recovered);
}

#[def_test]
fn invalid_orphan_head_recovery_failure_preserves_evidence_and_retries() {
    let (filesystem, device) = recovery_test_filesystem();
    drop(filesystem);
    let mut bytes = device.bytes();
    // A reserved chain head must come from corrupt on-disk input, not the
    // validated mutation API.
    bytes[1256..1260].copy_from_slice(&5u32.to_le_bytes());
    let device = Arc::new(TestDevice::new(bytes));

    device.fail_flush_at(device.flush_count() + 1);
    let recovery_device: Arc<dyn BlockDeviceOperations> = device.clone();
    assert_eq!(
        Ext4SbInfo::recover(recovery_device),
        Err(Ext4Error::Device(DriverError::Io))
    );

    {
        let bytes = device.bytes();
        let persisted =
            Ext4DiskSuperblock::decode(&bytes[1024..1024 + superblock::SUPERBLOCK_SIZE])
                .expect("decode failed invalid-orphan recovery superblock");
        assert_eq!(persisted.last_orphan(), 5);
        assert!(persisted.features().needs_recovery());
    }

    let mount_device: Arc<dyn BlockDeviceOperations> = device.clone();
    assert_eq!(
        open_test_filesystem(mount_device).map(|_| ()),
        Err(Ext4Error::NeedsRecovery)
    );

    let retry_device: Arc<dyn BlockDeviceOperations> = device.clone();
    Ext4SbInfo::recover(retry_device).expect("retry invalid orphan head recovery");
    let recovered_device: Arc<dyn BlockDeviceOperations> = device.clone();
    let recovered =
        open_test_filesystem(recovered_device).expect("mount retried orphan recovery image");
    assert_eq!(recovered.orphan_head(), None);
    assert!(!recovered.superblock().features().needs_recovery());
}

#[def_test]
fn recovery_rebases_transaction_sequence_before_regular_orphan_cleanup() {
    let (mut filesystem, device) = recovery_test_filesystem();
    let free_inodes_before_allocation = filesystem.free_inodes_count();
    let inode = allocate_checkpointed_regular_inode(&mut filesystem);
    let journal = filesystem.metadata_journal().expect("open mounted journal");
    let mut handle = journal.begin(JournalCredits::new(8)).unwrap();
    let transaction = handle.id();
    filesystem.add_orphan(&inode, &mut handle).unwrap();
    update_test_inode_links_count(&mut filesystem, &inode, 0, &mut handle);
    drop(handle);
    let commit = journal.transactions.force_commit(transaction).unwrap();
    filesystem
        .persist_metadata_journal_commit(&commit)
        .expect("persist orphan transaction without checkpoint");
    drop(filesystem);

    let recovery_device: Arc<dyn BlockDeviceOperations> = device.clone();
    let report = Ext4SbInfo::recover(recovery_device)
        .expect("replay and orphan cleanup")
        .expect("active journal report");
    assert!(report.update_count() > 0);

    let mount_device: Arc<dyn BlockDeviceOperations> = device.clone();
    let recovered = open_test_filesystem(mount_device).expect("mount recovered image");
    assert_eq!(recovered.orphan_head(), None);
    assert_eq!(recovered.free_inodes_count(), free_inodes_before_allocation);
    assert!(!recovered.needs_recovery());
}

#[def_test]
fn recover_keeps_recovery_feature_when_regular_orphan_cleanup_fails() {
    let (mut filesystem, device) = recovery_test_filesystem();
    let inode = allocate_checkpointed_directory_inode(&mut filesystem);
    let journal = filesystem.metadata_journal().expect("open mounted journal");
    let mut handle = journal.begin(JournalCredits::new(4)).unwrap();
    let transaction = handle.id();
    filesystem
        .set_orphan_head(Some(inode.number()), &mut handle)
        .unwrap();
    drop(handle);
    let commit = journal.transactions.force_commit(transaction).unwrap();
    filesystem
        .persist_metadata_journal_commit(&commit)
        .expect("persist orphan head update to journal");
    drop(filesystem);

    let recovery_device: Arc<dyn BlockDeviceOperations> = device.clone();
    assert_eq!(
        Ext4SbInfo::recover(recovery_device),
        Err(Ext4Error::Unsupported(UnsupportedKind::InodeKind))
    );

    let bytes = device.bytes();
    let superblock = Ext4DiskSuperblock::decode(&bytes[1024..1024 + superblock::SUPERBLOCK_SIZE])
        .expect("decode recovery-failed superblock");
    assert!(superblock.features().needs_recovery());
    assert_eq!(superblock.last_orphan(), inode.number().get());
}

#[def_test]
fn recovery_selection_uses_ext4_flag_and_clears_empty_journal() {
    let (filesystem, device) = recovery_test_filesystem();
    let journal_block = filesystem.map_journal_block(JournalBlock::new(0)).unwrap();
    drop(filesystem);
    let mut bytes = device.bytes();
    let incompat = le_u32(&bytes, 1024 + 0x60);
    put_u32(
        &mut bytes,
        1024 + 0x60,
        incompat | features::IncompatFeatures::RECOVER.bits(),
    );
    let device = Arc::new(TestDevice::new(bytes));
    assert!(matches!(
        open_test_filesystem(device.clone()),
        Err(Ext4Error::NeedsRecovery)
    ));
    let report = Ext4SbInfo::recover(device.clone()).unwrap().unwrap();
    assert_eq!(report.update_count(), 0);
    let recovered = open_test_filesystem(device.clone()).unwrap();
    assert!(!recovered.needs_recovery());
    let status = journal_status(&recovered);
    assert_eq!(status.start_block(), None);
    assert_eq!(status.sequence(), report.next_sequence());
    assert_eq!(status.head(), report.head());
    drop(recovered);

    // A stale JBD2 start without EXT4_RECOVER does not request replay.
    let mut bytes = device.bytes();
    let offset = journal_block.get() as usize * TEST_BLOCK_SIZE;
    let first = u32::from_be_bytes(bytes[offset + 0x14..offset + 0x18].try_into().unwrap());
    put_be_u32(&mut bytes, offset + 0x1c, first);
    let mounted = open_test_filesystem(Arc::new(TestDevice::new(bytes))).unwrap();
    assert_eq!(journal_status(&mounted).start_block(), Some(first));
    assert!(!mounted.needs_recovery());
}

fn pending_two_block_commit() -> Vec<u8> {
    let (mut filesystem, device) = recovery_test_filesystem();
    let journal = filesystem.metadata_journal().unwrap();
    let mut handle = journal.begin(JournalCredits::new(2)).unwrap();
    let transaction = handle.id();
    for (block, value) in [(8, 0x51), (9, 0x62)] {
        let access = filesystem
            .metadata_io
            .write_access(FilesystemBlock::new(block), &mut handle)
            .unwrap();
        access
            .update_bytes(|_, bytes| {
                bytes.fill(value);
                Ok(())
            })
            .unwrap();
        access.mark_dirty().unwrap();
    }
    drop(handle);
    let commit = journal.transactions.force_commit(transaction).unwrap();
    assert_eq!(commit.metadata_blocks().unwrap().len(), 2);
    filesystem.persist_metadata_journal_commit(&commit).unwrap();
    let bytes = device.bytes();
    for block in [8, 9] {
        assert!(
            bytes[block * TEST_BLOCK_SIZE..(block + 1) * TEST_BLOCK_SIZE]
                .iter()
                .all(|byte| *byte == 0)
        );
    }
    bytes
}

fn recovery_flag(bytes: &[u8]) -> bool {
    Ext4DiskSuperblock::decode(&bytes[1024..2048])
        .unwrap()
        .features()
        .needs_recovery()
}

#[def_test]
fn partial_replay_write_failure_keeps_completed_block_and_recovery_evidence() {
    use crate::superblock::fault_device::WriteThroughFaultDevice;
    let bytes = pending_two_block_commit();
    let device = Arc::new(WriteThroughFaultDevice::fail_write_at(bytes, 2));
    assert!(matches!(
        Ext4SbInfo::recover(device.clone()),
        Err(Ext4Error::Device(DriverError::Io))
    ));
    let committed = device.committed_bytes();
    assert!(recovery_flag(&committed));
    assert!(
        committed[8 * TEST_BLOCK_SIZE..9 * TEST_BLOCK_SIZE]
            .iter()
            .all(|byte| *byte == 0x51)
    );
    assert!(
        committed[9 * TEST_BLOCK_SIZE..10 * TEST_BLOCK_SIZE]
            .iter()
            .all(|byte| *byte == 0)
    );
    let retry = Arc::new(TestDevice::new(committed));
    let report = Ext4SbInfo::recover(retry.clone()).unwrap().unwrap();
    assert_eq!(report.update_count(), 2);
    assert_eq!(report.revoke_hit_count(), 0);
    let recovered = open_test_filesystem(retry.clone()).unwrap();
    assert_eq!(
        journal_status(&recovered).sequence(),
        report.next_sequence()
    );
    assert_eq!(journal_status(&recovered).head(), report.head());
    assert_eq!(journal_status(&recovered).start_block(), None);
    let bytes = retry.bytes();
    assert!(
        bytes[9 * TEST_BLOCK_SIZE..10 * TEST_BLOCK_SIZE]
            .iter()
            .all(|byte| *byte == 0x62)
    );
}

#[def_test]
fn replay_barrier_failures_preserve_durable_blocks_and_recovery_evidence() {
    use crate::superblock::fault_device::BufferedFaultDevice;
    let bytes = pending_two_block_commit();
    // FilesystemDevice submits each whole filesystem block in one request.
    let writes_per_block = 1;
    for (stage, write, flush, has_replayed_data) in [
        ("replay flush", None, Some(1), false),
        (
            "journal empty write",
            Some(2 * writes_per_block + 1),
            None,
            true,
        ),
        (
            "ext4 clear write",
            Some(3 * writes_per_block + 1),
            None,
            true,
        ),
        ("ext4 clear flush", None, Some(3), true),
    ] {
        let device = Arc::new(BufferedFaultDevice::new(bytes.clone(), write, flush));
        assert!(
            matches!(
                Ext4SbInfo::recover(device.clone()),
                Err(Ext4Error::Device(DriverError::Io))
            ),
            "{stage}"
        );
        let committed = device.committed_bytes();
        assert!(recovery_flag(&committed), "{stage}");
        for (block, expected) in [(8, 0x51), (9, 0x62)] {
            let expected = if has_replayed_data { expected } else { 0 };
            assert!(
                committed[block * TEST_BLOCK_SIZE..(block + 1) * TEST_BLOCK_SIZE]
                    .iter()
                    .all(|byte| *byte == expected),
                "{stage}"
            );
        }
    }
}

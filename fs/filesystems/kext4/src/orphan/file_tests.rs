// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec;

use unittest::def_test;

use super::*;
use crate::{
    Ext4Timestamp,
    disk::inode::EXT4_EXTENTS_FL,
    jbd2::{JournalTransactions, TransactionId},
    superblock::test_fixtures::{checksummed_orphan_test_filesystem, orphan_test_filesystem},
};

const TEST_ORPHAN_BLOCK: FilesystemBlock = FilesystemBlock::new(6);

fn install_orphan_block(
    filesystem: &mut Ext4SbInfo,
    checksum_seed: u32,
    entry: Option<InodeNumber>,
) {
    assert!(!filesystem.is_system_zone_block(TEST_ORPHAN_BLOCK));
    let entries_bytes = filesystem.orphan_entries_bytes();
    let entries_per_block = filesystem.orphan_entries_per_block();
    let mut bytes = vec![0; filesystem.device.block_size()];
    if let Some(entry) = entry {
        for slot in bytes[..entries_bytes].chunks_exact_mut(4) {
            slot.copy_from_slice(&entry.get().to_le_bytes());
        }
    }
    bytes[entries_bytes..entries_bytes + 4].copy_from_slice(&EXT4_ORPHAN_BLOCK_MAGIC.to_le_bytes());
    update_orphan_block_checksum(
        TEST_ORPHAN_BLOCK,
        checksum_seed,
        &mut bytes,
        filesystem.superblock().features().has_metadata_checksum(),
    )
    .unwrap();
    filesystem
        .write_contiguous_blocks(TEST_ORPHAN_BLOCK, 1, &bytes)
        .unwrap();
    filesystem.metadata_io.invalidate_all();
    filesystem.orphan_info = Some(OrphanInfo {
        checksum_seed,
        blocks: vec![OrphanBlock {
            physical: TEST_ORPHAN_BLOCK,
            free_entries: if entry.is_some() {
                0
            } else {
                entries_per_block
            },
        }],
    });
}

fn checkpoint(filesystem: &Ext4SbInfo, journal: &JournalTransactions, transaction: TransactionId) {
    let commit = journal.force_commit(transaction).unwrap();
    filesystem
        .metadata_io
        .checkpoint_committed(&commit)
        .unwrap();
    journal.finish_checkpoint_for_test(&commit).unwrap();
}

#[def_test]
fn orphan_block_checksum_matches_linux_seed_and_block_order() {
    let entries = [1, 0, 0, 0, 0x78, 0x56, 0x34, 0x12, 0, 0, 0, 0];

    assert_eq!(
        orphan_block_checksum(
            0x1122_3344,
            FilesystemBlock::new(0x0102_0304_0506_0708),
            &entries,
        ),
        0x7cb5_bbf1
    );
}

#[def_test]
fn orphan_file_empty_rejects_uninitialized_feature_state() {
    let (filesystem, _inode) = orphan_test_filesystem();

    assert_eq!(
        filesystem.orphan_file_empty(),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidOrphanFile))
    );
}

#[def_test]
fn orphan_file_add_remove_updates_slot_checksum_and_free_count() {
    let (mut filesystem, inode) = checksummed_orphan_test_filesystem();
    let checksum_seed = 0x89ab_cdef;
    install_orphan_block(&mut filesystem, checksum_seed, None);
    let entries_bytes = filesystem.orphan_entries_bytes();
    let entries_per_block = filesystem.orphan_entries_per_block();
    assert_eq!(filesystem.orphan_file_empty(), Ok(true));

    let journal = JournalTransactions::new(TransactionId::new(1000));
    let mut handle = journal.begin(JournalCredits::new(4)).unwrap();
    let transaction = handle.id();
    filesystem.add_orphan(&inode, &mut handle).unwrap();
    drop(handle);
    checkpoint(&filesystem, &journal, transaction);

    assert_eq!(
        inode.orphan_location(),
        Some(OrphanLocation::File { index: 0 })
    );
    assert_eq!(filesystem.orphan_file_empty(), Ok(false));
    assert_eq!(
        filesystem.orphan_info.as_ref().unwrap().blocks[0].free_entries,
        entries_per_block - 1
    );
    let block = filesystem.read_metadata_block(TEST_ORPHAN_BLOCK).unwrap();
    assert_eq!(codec::le_u32(block.as_ref(), 0), Ok(inode.number().get()));
    assert_eq!(
        codec::le_u32(block.as_ref(), entries_bytes + 4),
        Ok(orphan_block_checksum(
            checksum_seed,
            TEST_ORPHAN_BLOCK,
            &block.as_ref()[..entries_bytes],
        ))
    );
    drop(block);

    let mut handle = journal.begin(JournalCredits::new(4)).unwrap();
    let transaction = handle.id();
    filesystem.remove_orphan(&inode, &mut handle).unwrap();
    drop(handle);
    checkpoint(&filesystem, &journal, transaction);

    assert_eq!(inode.orphan_location(), None);
    assert_eq!(filesystem.orphan_file_empty(), Ok(true));
    assert_eq!(
        filesystem.orphan_info.as_ref().unwrap().blocks[0].free_entries,
        entries_per_block
    );
    let block = filesystem.read_metadata_block(TEST_ORPHAN_BLOCK).unwrap();
    assert_eq!(codec::le_u32(block.as_ref(), 0), Ok(0));
    assert_eq!(
        codec::le_u32(block.as_ref(), entries_bytes + 4),
        Ok(orphan_block_checksum(
            checksum_seed,
            TEST_ORPHAN_BLOCK,
            &block.as_ref()[..entries_bytes],
        ))
    );
}

#[def_test]
fn full_orphan_file_falls_back_to_legacy_list() {
    let (mut filesystem, inode) = orphan_test_filesystem();
    install_orphan_block(&mut filesystem, 0, Some(inode.number()));

    let journal = JournalTransactions::new(TransactionId::new(1002));
    let mut handle = journal.begin(JournalCredits::new(4)).unwrap();
    filesystem.add_orphan(&inode, &mut handle).unwrap();

    assert_eq!(inode.orphan_location(), Some(OrphanLocation::LegacyList));
    assert_eq!(filesystem.orphan_head(), Some(inode.number()));
}

#[def_test]
fn recovery_orphan_propagates_unsupported_inode_format() {
    let (mut filesystem, inode) = orphan_test_filesystem();
    let journal = JournalTransactions::new(TransactionId::new(1003));
    let mut handle = journal.begin(JournalCredits::new(2)).unwrap();
    let transaction = handle.id();
    filesystem
        .update_inode_flags_timestamps_metadata(
            &inode,
            inode.flags() & !EXT4_EXTENTS_FL,
            Ext4Timestamp::new(1003, 0),
            &mut handle,
        )
        .unwrap();
    // A legacy inode must contain block pointers, not the old extent header.
    let block = filesystem.inode_table_entry_block(inode.number()).unwrap();
    let access = filesystem
        .metadata_io
        .write_access(block, &mut handle)
        .unwrap();
    let mut bytes = metadata_access_bytes(&access).unwrap();
    filesystem
        .update_inode_table_entry_allow_zero_links(&mut bytes, inode.number(), |inode_bytes| {
            let start = crate::disk::inode::I_BLOCK_OFFSET;
            inode_bytes[start..start + crate::disk::inode::INODE_BLOCK_BYTES].fill(0);
            Ok(())
        })
        .unwrap();
    replace_metadata_access_bytes(&access, bytes).unwrap();
    drop(handle);
    checkpoint(&filesystem, &journal, transaction);

    assert!(matches!(
        filesystem.recovery_orphan(inode.number(), OrphanLocation::LegacyList),
        Err(Ext4Error::Unsupported(UnsupportedKind::NonExtentInode))
    ));
}

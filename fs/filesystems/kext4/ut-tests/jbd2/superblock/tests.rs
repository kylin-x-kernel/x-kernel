// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

const UUID: [u8; 16] = [0x5a; 16];
const FEATURE_INCOMPAT_ASYNC_COMMIT: u32 = 0x0000_0004;

#[def_test]
fn decodes_zero_start_v2_journal() {
    let bytes = valid_journal_superblock();
    let journal = JournalSuperblock::decode(&bytes, 4096, 1024, UUID).unwrap();

    assert_eq!(journal.max_blocks(), 1024);
    assert_eq!(journal.first_log_block(), JournalBlock::new(1));
    assert!(!journal.has_nonzero_log_start());
    assert_eq!(journal.encoded().len(), 4096);
    assert!(
        journal.encoded()[JOURNAL_SUPERBLOCK_SIZE..]
            .iter()
            .all(|byte| *byte == 0)
    );
}

#[def_test]
fn preserves_complete_journal_block_image() {
    let superblock = valid_journal_superblock();
    let mut block = [0xa5; 4096];
    block[..JOURNAL_SUPERBLOCK_SIZE].copy_from_slice(&superblock);

    let journal = JournalSuperblock::decode(&block, 4096, 1024, UUID).unwrap();

    assert_eq!(journal.encoded(), block.as_slice());
}

#[def_test]
fn preserves_nonzero_log_start_without_calling_it_recovery() {
    let mut bytes = valid_journal_superblock();
    put_be_u32(&mut bytes, 0x1c, 1);

    let journal = JournalSuperblock::decode(&bytes, 4096, 1024, UUID).unwrap();
    assert_eq!(journal.start(), JournalStart::Block(JournalBlock::new(1)));
    assert!(journal.has_nonzero_log_start());
}

#[def_test]
fn rejects_out_of_range_log_start() {
    let mut bytes = valid_journal_superblock();
    put_be_u32(&mut bytes, 0x1c, 1024);

    assert_eq!(
        JournalSuperblock::decode(&bytes, 4096, 1024, UUID),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal))
    );
}

#[def_test]
fn reports_complete_raw_feature_bitmaps() {
    let mut bytes = valid_journal_superblock();
    put_be_u32(&mut bytes, 0x24, 0x8000_0000);
    put_be_u32(&mut bytes, 0x28, FEATURE_INCOMPAT_FAST_COMMIT);
    put_be_u32(&mut bytes, 0x2c, 0x4000_0000);

    assert_eq!(
        JournalSuperblock::decode(&bytes, 4096, 1024, UUID),
        Err(Ext4Error::UnsupportedJournalFeature {
            compat: 0x8000_0000,
            incompat: FEATURE_INCOMPAT_FAST_COMMIT,
            read_only_compat: 0x4000_0000,
        })
    );
}

#[def_test]
fn rejects_async_commit_until_recovery_semantics_are_supported() {
    let mut bytes = valid_journal_superblock();
    put_be_u32(&mut bytes, 0x28, FEATURE_INCOMPAT_ASYNC_COMMIT);

    assert_eq!(
        JournalSuperblock::decode(&bytes, 4096, 1024, UUID),
        Err(Ext4Error::UnsupportedJournalFeature {
            compat: 0,
            incompat: FEATURE_INCOMPAT_ASYNC_COMMIT,
            read_only_compat: 0,
        })
    );
}

#[def_test]
fn rejects_conflicting_checksum_features() {
    let mut bytes = valid_journal_superblock();
    put_be_u32(&mut bytes, 0x24, FEATURE_COMPAT_CHECKSUM);
    put_be_u32(&mut bytes, 0x28, FEATURE_INCOMPAT_CSUM_V3);

    assert_eq!(
        JournalSuperblock::decode(&bytes, 4096, 1024, UUID),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal))
    );
}

#[def_test]
fn verifies_v3_superblock_checksum() {
    let mut bytes = valid_journal_superblock();
    put_be_u32(&mut bytes, 0x28, FEATURE_INCOMPAT_CSUM_V3);
    bytes[0x50] = JBD2_CRC32C_CHECKSUM;
    let checksum = journal_superblock_checksum(&bytes).unwrap();
    put_be_u32(&mut bytes, CHECKSUM_OFFSET, checksum);
    assert!(JournalSuperblock::decode(&bytes, 4096, 1024, UUID).is_ok());

    bytes[0x60] ^= 1;
    assert!(matches!(
        JournalSuperblock::decode(&bytes, 4096, 1024, UUID),
        Err(Ext4Error::ChecksumMismatch {
            target: ChecksumTarget::JournalSuperblock,
            ..
        })
    ));
}

#[def_test]
fn marks_journal_superblock_empty() {
    let mut bytes = valid_journal_superblock();
    put_be_u32(&mut bytes, START_OFFSET, 7);

    mark_superblock_empty(&mut bytes, TransactionId::new(9), JournalBlock::new(8)).unwrap();

    let journal = JournalSuperblock::decode(&bytes, 4096, 1024, UUID).unwrap();
    assert_eq!(journal.sequence(), TransactionId::new(9));
    assert_eq!(journal.start(), JournalStart::Zero);
    assert_eq!(journal.head(), JournalBlock::new(8));
}

#[def_test]
fn enables_revoke_feature_on_v2_journal() {
    let mut bytes = valid_journal_superblock();

    assert!(enable_superblock_revoke(&mut bytes).unwrap());

    let journal = JournalSuperblock::decode(&bytes, 4096, 1024, UUID).unwrap();
    assert_ne!(journal.feature_incompat() & FEATURE_INCOMPAT_REVOKE, 0);
}

#[def_test]
fn enabling_revoke_updates_v3_superblock_checksum() {
    let mut bytes = valid_journal_superblock();
    put_be_u32(&mut bytes, 0x28, FEATURE_INCOMPAT_CSUM_V3);
    bytes[0x50] = JBD2_CRC32C_CHECKSUM;
    let checksum = journal_superblock_checksum(&bytes).unwrap();
    put_be_u32(&mut bytes, CHECKSUM_OFFSET, checksum);

    assert!(enable_superblock_revoke(&mut bytes).unwrap());

    let journal = JournalSuperblock::decode(&bytes, 4096, 1024, UUID).unwrap();
    assert_ne!(journal.feature_incompat() & FEATURE_INCOMPAT_REVOKE, 0);
}

#[def_test]
fn enabling_revoke_rejects_active_journal() {
    let mut bytes = valid_journal_superblock();
    put_be_u32(&mut bytes, START_OFFSET, 1);

    assert_eq!(
        enable_superblock_revoke(&mut bytes),
        Err(Ext4Error::JournalBusy)
    );
}

#[def_test]
fn leaves_v1_journal_without_feature_bitmap_unchanged() {
    let mut bytes = valid_journal_superblock();
    put_be_u32(&mut bytes, 0x04, JBD2_SUPERBLOCK_V1);
    let before = bytes;

    assert!(!enable_superblock_revoke(&mut bytes).unwrap());
    assert_eq!(bytes, before);
}

#[def_test]
fn marks_journal_superblock_active() {
    let mut bytes = valid_journal_superblock();
    // Activating the tail preserves the existing persistent head.
    put_be_u32(&mut bytes, HEAD_OFFSET, 8);

    mark_superblock_active(&mut bytes, TransactionId::new(9), JournalBlock::new(7)).unwrap();

    let journal = JournalSuperblock::decode(&bytes, 4096, 1024, UUID).unwrap();
    assert_eq!(journal.sequence(), TransactionId::new(9));
    assert_eq!(journal.start(), JournalStart::Block(JournalBlock::new(7)));
    assert_eq!(journal.head(), JournalBlock::new(8));
}

#[def_test]
fn active_journal_superblock_rejects_out_of_range_log_positions() {
    let mut bytes = valid_journal_superblock();

    assert_eq!(
        mark_superblock_active(&mut bytes, TransactionId::new(9), JournalBlock::new(0),),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal))
    );
}

#[def_test]
fn marks_v3_journal_superblock_empty_and_updates_checksum() {
    let mut bytes = valid_journal_superblock();
    put_be_u32(&mut bytes, START_OFFSET, 7);
    put_be_u32(&mut bytes, 0x28, FEATURE_INCOMPAT_CSUM_V3);
    bytes[0x50] = JBD2_CRC32C_CHECKSUM;
    let checksum = journal_superblock_checksum(&bytes).unwrap();
    put_be_u32(&mut bytes, CHECKSUM_OFFSET, checksum);

    mark_superblock_empty(&mut bytes, TransactionId::new(9), JournalBlock::new(8)).unwrap();

    let journal = JournalSuperblock::decode(&bytes, 4096, 1024, UUID).unwrap();
    assert_eq!(journal.sequence(), TransactionId::new(9));
    assert_eq!(journal.start(), JournalStart::Zero);
    assert_eq!(journal.head(), JournalBlock::new(8));
}

#[def_test]
fn marks_v3_journal_superblock_active_and_updates_checksum() {
    let mut bytes = valid_journal_superblock();
    // Activating the tail preserves the existing persistent head.
    put_be_u32(&mut bytes, HEAD_OFFSET, 8);
    put_be_u32(&mut bytes, 0x28, FEATURE_INCOMPAT_CSUM_V3);
    bytes[0x50] = JBD2_CRC32C_CHECKSUM;
    let checksum = journal_superblock_checksum(&bytes).unwrap();
    put_be_u32(&mut bytes, CHECKSUM_OFFSET, checksum);

    mark_superblock_active(&mut bytes, TransactionId::new(9), JournalBlock::new(7)).unwrap();

    let journal = JournalSuperblock::decode(&bytes, 4096, 1024, UUID).unwrap();
    assert_eq!(journal.sequence(), TransactionId::new(9));
    assert_eq!(journal.start(), JournalStart::Block(JournalBlock::new(7)));
    assert_eq!(journal.head(), JournalBlock::new(8));
}

#[def_test]
fn marks_v3_journal_superblock_empty_inside_larger_filesystem_block() {
    let mut superblock = valid_journal_superblock();
    put_be_u32(&mut superblock, START_OFFSET, 7);
    put_be_u32(&mut superblock, 0x28, FEATURE_INCOMPAT_CSUM_V3);
    superblock[0x50] = JBD2_CRC32C_CHECKSUM;
    let checksum = journal_superblock_checksum(&superblock).unwrap();
    put_be_u32(&mut superblock, CHECKSUM_OFFSET, checksum);

    let mut block = [0xa5; 4096];
    block[..JOURNAL_SUPERBLOCK_SIZE].copy_from_slice(&superblock);

    mark_superblock_empty(&mut block, TransactionId::new(9), JournalBlock::new(8)).unwrap();

    let journal =
        JournalSuperblock::decode(&block, 4096, 1024, UUID).expect("decode updated journal");
    assert_eq!(journal.sequence(), TransactionId::new(9));
    assert_eq!(journal.start(), JournalStart::Zero);
    assert_eq!(journal.head(), JournalBlock::new(8));
    assert!(
        block[JOURNAL_SUPERBLOCK_SIZE..]
            .iter()
            .all(|byte| *byte == 0xa5)
    );
}

fn valid_journal_superblock() -> [u8; JOURNAL_SUPERBLOCK_SIZE] {
    let mut bytes = [0; JOURNAL_SUPERBLOCK_SIZE];
    put_be_u32(&mut bytes, 0x00, JBD2_MAGIC_NUMBER);
    put_be_u32(&mut bytes, 0x04, JBD2_SUPERBLOCK_V2);
    put_be_u32(&mut bytes, 0x0c, 4096);
    put_be_u32(&mut bytes, 0x10, 1024);
    put_be_u32(&mut bytes, 0x14, 1);
    put_be_u32(&mut bytes, 0x18, 1);
    bytes[0x30..0x40].copy_from_slice(&UUID);
    put_be_u32(&mut bytes, 0x40, 1);
    bytes
}

fn put_be_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
}

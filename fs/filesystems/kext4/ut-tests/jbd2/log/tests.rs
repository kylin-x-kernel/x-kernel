// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::collections::BTreeMap;

use unittest::def_test;

use super::*;
use crate::jbd2::superblock::JOURNAL_SUPERBLOCK_SIZE;

const UUID: [u8; 16] = [0x5a; 16];
const BLOCK_SIZE: usize = 1024;
const SEQUENCE: u32 = 7;

struct TestJournal {
    blocks: BTreeMap<u32, Vec<u8>>,
}

impl TestJournal {
    fn new() -> Self {
        Self {
            blocks: BTreeMap::new(),
        }
    }

    fn insert(&mut self, block: u32, bytes: Vec<u8>) {
        self.blocks.insert(block, bytes);
    }
}

impl JournalBlockReader for TestJournal {
    fn read_journal_block(&self, block: JournalBlock, output: &mut [u8]) -> Ext4Result<()> {
        let bytes = self
            .blocks
            .get(&block.get())
            .ok_or(Ext4Error::OutOfBounds)?;
        if bytes.len() != output.len() {
            return Err(Ext4Error::InvalidBufferLength {
                expected: output.len(),
                actual: bytes.len(),
            });
        }
        output.copy_from_slice(bytes);
        Ok(())
    }
}

#[def_test]
fn scans_committed_updates_and_revokes() {
    let superblock = journal_superblock(1, FEATURE_INCOMPAT_REVOKE);
    let mut journal = TestJournal::new();

    let mut descriptor = block_header(JBD2_DESCRIPTOR_BLOCK, SEQUENCE);
    put_be_u32(&mut descriptor, 12, 100);
    put_be_u16(&mut descriptor, 18, 0);
    descriptor[20..36].copy_from_slice(&UUID);
    put_be_u32(&mut descriptor, 36, 200);
    put_be_u16(
        &mut descriptor,
        42,
        (JBD2_FLAG_SAME_UUID | JBD2_FLAG_LAST_TAG) as u16,
    );
    journal.insert(1, descriptor);
    journal.insert(2, vec![0x11; BLOCK_SIZE]);
    journal.insert(3, vec![0x22; BLOCK_SIZE]);

    let mut revoke = block_header(JBD2_REVOKE_BLOCK, SEQUENCE);
    put_be_u32(&mut revoke, 12, 20);
    put_be_u32(&mut revoke, 16, 100);
    journal.insert(4, revoke);
    journal.insert(5, block_header(JBD2_COMMIT_BLOCK, SEQUENCE));
    journal.insert(6, vec![0; BLOCK_SIZE]);

    let scan = scan_journal(&superblock, &journal).unwrap();
    assert_eq!(scan.head(), JournalBlock::new(6));
    assert_eq!(scan.next_sequence(), TransactionId::new(SEQUENCE + 1));
    assert_eq!(scan.transactions().len(), 1);

    let transaction = &scan.transactions()[0];
    assert_eq!(transaction.state(), JournalTransactionState::Committed);
    assert_eq!(transaction.start(), JournalBlock::new(1));
    assert_eq!(transaction.next_log_block(), JournalBlock::new(6));
    assert_eq!(
        transaction.updates(),
        &[
            JournalUpdate {
                target: JournalTargetBlock::new(100),
                log_block: JournalBlock::new(2),
                is_escaped: false,
            },
            JournalUpdate {
                target: JournalTargetBlock::new(200),
                log_block: JournalBlock::new(3),
                is_escaped: false,
            },
        ]
    );
    assert_eq!(
        transaction.revoked_blocks(),
        &[JournalTargetBlock::new(100)]
    );
}

#[def_test]
fn rejects_revoke_block_without_revoke_feature() {
    let superblock = journal_superblock(1, 0);
    let mut journal = TestJournal::new();
    let mut revoke = block_header(JBD2_REVOKE_BLOCK, SEQUENCE);
    put_be_u32(&mut revoke, 12, 20);
    put_be_u32(&mut revoke, 16, 100);
    journal.insert(1, revoke);

    assert_eq!(
        scan_journal(&superblock, &journal),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal))
    );
}

#[def_test]
fn leaves_transaction_uncommitted_at_sequence_boundary() {
    let superblock = journal_superblock(1, 0);
    let mut journal = TestJournal::new();
    journal.insert(1, one_tag_descriptor(300, 0));
    journal.insert(2, vec![0x33; BLOCK_SIZE]);
    journal.insert(3, block_header(JBD2_DESCRIPTOR_BLOCK, SEQUENCE + 1));

    let scan = scan_journal(&superblock, &journal).unwrap();
    assert_eq!(scan.head(), JournalBlock::new(3));
    assert_eq!(scan.next_sequence(), TransactionId::new(SEQUENCE));
    assert_eq!(scan.transactions().len(), 1);
    assert_eq!(
        scan.transactions()[0].state(),
        JournalTransactionState::Uncommitted
    );
    assert_eq!(
        scan.transactions()[0].next_log_block(),
        JournalBlock::new(3)
    );
}

#[def_test]
fn wraps_descriptor_data_and_commit_across_log_end() {
    let superblock = journal_superblock(1023, 0);
    let mut journal = TestJournal::new();
    journal.insert(1023, one_tag_descriptor(400, 0));
    journal.insert(1, vec![0x44; BLOCK_SIZE]);
    journal.insert(2, block_header(JBD2_COMMIT_BLOCK, SEQUENCE));
    journal.insert(3, vec![0; BLOCK_SIZE]);

    let scan = scan_journal(&superblock, &journal).unwrap();
    let transaction = &scan.transactions()[0];
    assert_eq!(transaction.state(), JournalTransactionState::Committed);
    assert_eq!(transaction.updates()[0].log_block(), JournalBlock::new(1));
    assert_eq!(transaction.next_log_block(), JournalBlock::new(3));
}

#[def_test]
fn verifies_v3_control_and_data_checksums() {
    let superblock = journal_superblock(1, FEATURE_INCOMPAT_CSUM_V3);
    let checksum_seed = checksum::crc32c(u32::MAX, &UUID);
    let mut journal = TestJournal::new();
    let data = vec![0x55; BLOCK_SIZE];

    let mut descriptor = block_header(JBD2_DESCRIPTOR_BLOCK, SEQUENCE);
    put_be_u32(&mut descriptor, 12, 500);
    put_be_u32(&mut descriptor, 16, JBD2_FLAG_LAST_TAG);
    let data_checksum = checksum::crc32c(checksum_seed, &SEQUENCE.to_be_bytes());
    let data_checksum = checksum::crc32c(data_checksum, &data);
    put_be_u32(&mut descriptor, 24, data_checksum);
    set_block_tail_checksum(&mut descriptor, checksum_seed);
    journal.insert(1, descriptor);
    journal.insert(2, data);

    let mut commit = block_header(JBD2_COMMIT_BLOCK, SEQUENCE);
    let commit_checksum = checksum_with_zeroed_u32(checksum_seed, &commit, 16).unwrap();
    put_be_u32(&mut commit, 16, commit_checksum);
    journal.insert(3, commit);
    journal.insert(4, vec![0; BLOCK_SIZE]);

    let scan = scan_journal(&superblock, &journal).unwrap();
    assert_eq!(
        scan.transactions()[0].state(),
        JournalTransactionState::Committed
    );

    journal.blocks.get_mut(&2).unwrap()[0] ^= 1;
    assert!(matches!(
        scan_journal(&superblock, &journal),
        Err(Ext4Error::ChecksumMismatch {
            target: ChecksumTarget::JournalBlock(2),
            ..
        })
    ));
}

#[def_test]
fn rejects_v3_descriptor_checksum_mismatch() {
    let superblock = journal_superblock(1, FEATURE_INCOMPAT_CSUM_V3);
    let checksum_seed = checksum::crc32c(u32::MAX, &UUID);
    let mut journal = TestJournal::new();
    let data = vec![0x57; BLOCK_SIZE];

    let mut descriptor = v3_descriptor_with_one_tag(500, SEQUENCE, checksum_seed, &data);
    descriptor[12] ^= 1;
    journal.insert(1, descriptor);
    journal.insert(2, data);
    journal.insert(3, v3_commit(SEQUENCE, checksum_seed));
    journal.insert(4, vec![0; BLOCK_SIZE]);

    assert!(matches!(
        scan_journal(&superblock, &journal),
        Err(Ext4Error::ChecksumMismatch {
            target: ChecksumTarget::JournalBlock(1),
            ..
        })
    ));
}

#[def_test]
fn rejects_v3_commit_checksum_mismatch() {
    let superblock = journal_superblock(1, FEATURE_INCOMPAT_CSUM_V3);
    let checksum_seed = checksum::crc32c(u32::MAX, &UUID);
    let mut journal = TestJournal::new();
    let data = vec![0x58; BLOCK_SIZE];

    journal.insert(
        1,
        v3_descriptor_with_one_tag(500, SEQUENCE, checksum_seed, &data),
    );
    journal.insert(2, data);
    let mut commit = v3_commit(SEQUENCE, checksum_seed);
    commit[16] ^= 1;
    journal.insert(3, commit);
    journal.insert(4, vec![0; BLOCK_SIZE]);

    assert!(matches!(
        scan_journal(&superblock, &journal),
        Err(Ext4Error::ChecksumMismatch {
            target: ChecksumTarget::JournalBlock(3),
            ..
        })
    ));
}

#[def_test]
fn rejects_v3_revoke_checksum_mismatch() {
    let incompat = FEATURE_INCOMPAT_REVOKE | FEATURE_INCOMPAT_CSUM_V3;
    let superblock = journal_superblock(1, incompat);
    let checksum_seed = checksum::crc32c(u32::MAX, &UUID);
    let mut journal = TestJournal::new();

    let mut revoke = block_header(JBD2_REVOKE_BLOCK, SEQUENCE);
    put_be_u32(&mut revoke, 12, 20);
    put_be_u32(&mut revoke, 16, 700);
    set_block_tail_checksum(&mut revoke, checksum_seed);
    revoke[16] ^= 1;
    journal.insert(1, revoke);
    journal.insert(2, v3_commit(SEQUENCE, checksum_seed));
    journal.insert(3, vec![0; BLOCK_SIZE]);

    assert!(matches!(
        scan_journal(&superblock, &journal),
        Err(Ext4Error::ChecksumMismatch {
            target: ChecksumTarget::JournalBlock(1),
            ..
        })
    ));
}

#[def_test]
fn rejects_v2_revoke_checksum_mismatch() {
    let incompat = FEATURE_INCOMPAT_REVOKE | FEATURE_INCOMPAT_CSUM_V2;
    let superblock = journal_superblock(1, incompat);
    let checksum_seed = checksum::crc32c(u32::MAX, &UUID);
    let mut journal = TestJournal::new();

    let mut revoke = block_header(JBD2_REVOKE_BLOCK, SEQUENCE);
    put_be_u32(&mut revoke, 12, 20);
    put_be_u32(&mut revoke, 16, 700);
    set_block_tail_checksum(&mut revoke, checksum_seed);
    revoke[16] ^= 1;
    journal.insert(1, revoke);
    journal.insert(2, checksummed_commit(SEQUENCE, checksum_seed));
    journal.insert(3, vec![0; BLOCK_SIZE]);

    assert!(matches!(
        scan_journal(&superblock, &journal),
        Err(Ext4Error::ChecksumMismatch {
            target: ChecksumTarget::JournalBlock(1),
            ..
        })
    ));
}

#[def_test]
fn scans_v3_checksum_tags_with_64bit_targets() {
    let incompat = FEATURE_INCOMPAT_CSUM_V3 | FEATURE_INCOMPAT_64BIT;
    let superblock = journal_superblock(1, incompat);
    let checksum_seed = checksum::crc32c(u32::MAX, &UUID);
    let mut journal = TestJournal::new();
    let data = vec![0x5f; BLOCK_SIZE];

    let mut descriptor = block_header(JBD2_DESCRIPTOR_BLOCK, SEQUENCE);
    put_be_u32(&mut descriptor, 12, 0x89ab_cdef);
    put_be_u32(&mut descriptor, 16, JBD2_FLAG_LAST_TAG);
    put_be_u32(&mut descriptor, 20, 2);
    let data_checksum = checksum::crc32c(checksum_seed, &SEQUENCE.to_be_bytes());
    let data_checksum = checksum::crc32c(data_checksum, &data);
    put_be_u32(&mut descriptor, 24, data_checksum);
    set_block_tail_checksum(&mut descriptor, checksum_seed);
    journal.insert(1, descriptor);
    journal.insert(2, data);

    let mut commit = block_header(JBD2_COMMIT_BLOCK, SEQUENCE);
    let commit_checksum = checksum_with_zeroed_u32(checksum_seed, &commit, 16).unwrap();
    put_be_u32(&mut commit, 16, commit_checksum);
    journal.insert(3, commit);
    journal.insert(4, vec![0; BLOCK_SIZE]);

    let scan = scan_journal(&superblock, &journal).unwrap();
    assert_eq!(
        scan.transactions()[0].updates()[0].target(),
        JournalTargetBlock::new(0x0000_0002_89ab_cdef)
    );
}

#[def_test]
fn scans_v2_checksum_tags_with_64bit_targets() {
    let incompat = FEATURE_INCOMPAT_CSUM_V2 | FEATURE_INCOMPAT_64BIT;
    let superblock = journal_superblock(1, incompat);
    let checksum_seed = checksum::crc32c(u32::MAX, &UUID);
    let mut journal = TestJournal::new();
    let data = vec![0x66; BLOCK_SIZE];

    let mut descriptor = block_header(JBD2_DESCRIPTOR_BLOCK, SEQUENCE);
    put_be_u32(&mut descriptor, 12, 0x1234_5678);
    let data_checksum = checksum::crc32c(checksum_seed, &SEQUENCE.to_be_bytes());
    let data_checksum = checksum::crc32c(data_checksum, &data);
    put_be_u16(&mut descriptor, 16, data_checksum as u16);
    put_be_u16(&mut descriptor, 18, JBD2_FLAG_LAST_TAG as u16);
    put_be_u32(&mut descriptor, 20, 1);
    descriptor[24..40].copy_from_slice(&UUID);
    set_block_tail_checksum(&mut descriptor, checksum_seed);
    journal.insert(1, descriptor);
    journal.insert(2, data);

    let mut commit = block_header(JBD2_COMMIT_BLOCK, SEQUENCE);
    let commit_checksum = checksum_with_zeroed_u32(checksum_seed, &commit, 16).unwrap();
    put_be_u32(&mut commit, 16, commit_checksum);
    journal.insert(3, commit);
    journal.insert(4, vec![0; BLOCK_SIZE]);

    let scan = scan_journal(&superblock, &journal).unwrap();
    assert_eq!(
        scan.transactions()[0].updates()[0].target(),
        JournalTargetBlock::new(0x0000_0001_1234_5678)
    );
}

#[def_test]
fn scans_v2_checksum_tags_with_32bit_targets_and_uuid() {
    let superblock = journal_superblock(1, FEATURE_INCOMPAT_CSUM_V2);
    let checksum_seed = checksum::crc32c(u32::MAX, &UUID);
    let mut journal = TestJournal::new();
    let data = vec![0x77; BLOCK_SIZE];

    let mut descriptor = block_header(JBD2_DESCRIPTOR_BLOCK, SEQUENCE);
    put_be_u32(&mut descriptor, 12, 0x1234_5678);
    let data_checksum = checksum::crc32c(checksum_seed, &SEQUENCE.to_be_bytes());
    let data_checksum = checksum::crc32c(data_checksum, &data);
    put_be_u16(&mut descriptor, 16, data_checksum as u16);
    put_be_u16(&mut descriptor, 18, JBD2_FLAG_LAST_TAG as u16);
    descriptor[20..36].copy_from_slice(&UUID);
    set_block_tail_checksum(&mut descriptor, checksum_seed);
    journal.insert(1, descriptor);
    journal.insert(2, data);

    let mut commit = block_header(JBD2_COMMIT_BLOCK, SEQUENCE);
    let commit_checksum = checksum_with_zeroed_u32(checksum_seed, &commit, 16).unwrap();
    put_be_u32(&mut commit, 16, commit_checksum);
    journal.insert(3, commit);
    journal.insert(4, vec![0; BLOCK_SIZE]);

    let scan = scan_journal(&superblock, &journal).unwrap();
    assert_eq!(
        scan.transactions()[0].updates()[0].target(),
        JournalTargetBlock::new(0x1234_5678)
    );
}

#[def_test]
fn scans_v2_checksum_same_uuid_tags_without_padding() {
    let superblock = journal_superblock(1, FEATURE_INCOMPAT_CSUM_V2);
    let checksum_seed = checksum::crc32c(u32::MAX, &UUID);
    let mut journal = TestJournal::new();
    let first_data = vec![0x88; BLOCK_SIZE];
    let second_data = vec![0x99; BLOCK_SIZE];

    let mut descriptor = block_header(JBD2_DESCRIPTOR_BLOCK, SEQUENCE);
    put_v2_32bit_tag(&mut descriptor, 12, 100, 0, checksum_seed, &first_data);
    descriptor[20..36].copy_from_slice(&UUID);
    put_v2_32bit_tag(
        &mut descriptor,
        36,
        200,
        (JBD2_FLAG_SAME_UUID | JBD2_FLAG_LAST_TAG) as u16,
        checksum_seed,
        &second_data,
    );
    set_block_tail_checksum(&mut descriptor, checksum_seed);
    journal.insert(1, descriptor);
    journal.insert(2, first_data);
    journal.insert(3, second_data);

    let mut commit = block_header(JBD2_COMMIT_BLOCK, SEQUENCE);
    let commit_checksum = checksum_with_zeroed_u32(checksum_seed, &commit, 16).unwrap();
    put_be_u32(&mut commit, 16, commit_checksum);
    journal.insert(4, commit);
    journal.insert(5, vec![0; BLOCK_SIZE]);

    let scan = scan_journal(&superblock, &journal).unwrap();
    assert_eq!(
        scan.transactions()[0]
            .updates()
            .iter()
            .map(|update| update.target())
            .collect::<Vec<_>>(),
        vec![JournalTargetBlock::new(100), JournalTargetBlock::new(200)]
    );
}

#[def_test]
fn scans_v2_checksum_64bit_same_uuid_tags_without_padding() {
    let incompat = FEATURE_INCOMPAT_CSUM_V2 | FEATURE_INCOMPAT_64BIT;
    let superblock = journal_superblock(1, incompat);
    let checksum_seed = checksum::crc32c(u32::MAX, &UUID);
    let mut journal = TestJournal::new();
    let first_data = vec![0x8a; BLOCK_SIZE];
    let second_data = vec![0x9a; BLOCK_SIZE];

    let mut descriptor = block_header(JBD2_DESCRIPTOR_BLOCK, SEQUENCE);
    put_v2_64bit_tag(
        &mut descriptor,
        12,
        0x0000_0001_1111_2222,
        0,
        checksum_seed,
        &first_data,
    );
    descriptor[24..40].copy_from_slice(&UUID);
    put_v2_64bit_tag(
        &mut descriptor,
        40,
        0x0000_0002_3333_4444,
        (JBD2_FLAG_SAME_UUID | JBD2_FLAG_LAST_TAG) as u16,
        checksum_seed,
        &second_data,
    );
    set_block_tail_checksum(&mut descriptor, checksum_seed);
    journal.insert(1, descriptor);
    journal.insert(2, first_data);
    journal.insert(3, second_data);

    let mut commit = block_header(JBD2_COMMIT_BLOCK, SEQUENCE);
    let commit_checksum = checksum_with_zeroed_u32(checksum_seed, &commit, 16).unwrap();
    put_be_u32(&mut commit, 16, commit_checksum);
    journal.insert(4, commit);
    journal.insert(5, vec![0; BLOCK_SIZE]);

    let scan = scan_journal(&superblock, &journal).unwrap();
    assert_eq!(
        scan.transactions()[0]
            .updates()
            .iter()
            .map(|update| update.target())
            .collect::<Vec<_>>(),
        vec![
            JournalTargetBlock::new(0x0000_0001_1111_2222),
            JournalTargetBlock::new(0x0000_0002_3333_4444),
        ]
    );
}

#[def_test]
fn scans_transaction_sequence_wraparound() {
    let superblock = journal_superblock_with_sequence(1, 0, u32::MAX);
    let mut journal = TestJournal::new();
    journal.insert(1, one_tag_descriptor_with_sequence(100, 0, u32::MAX));
    journal.insert(2, vec![0xaa; BLOCK_SIZE]);
    journal.insert(3, block_header(JBD2_COMMIT_BLOCK, u32::MAX));
    journal.insert(4, one_tag_descriptor_with_sequence(200, 0, 0));
    journal.insert(5, vec![0xbb; BLOCK_SIZE]);
    journal.insert(6, block_header(JBD2_COMMIT_BLOCK, 0));
    journal.insert(7, vec![0; BLOCK_SIZE]);

    let scan = scan_journal(&superblock, &journal).unwrap();

    assert_eq!(scan.transactions().len(), 2);
    assert_eq!(scan.next_sequence(), TransactionId::new(1));
    assert_eq!(
        scan.transactions()[0].sequence(),
        TransactionId::new(u32::MAX)
    );
    assert_eq!(scan.transactions()[1].sequence(), TransactionId::new(0));
}

#[def_test]
fn rejects_first_tag_without_uuid() {
    let superblock = journal_superblock(1, 0);
    let mut journal = TestJournal::new();
    journal.insert(1, one_tag_descriptor(600, JBD2_FLAG_SAME_UUID as u16));

    assert_eq!(
        scan_journal(&superblock, &journal),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal))
    );
}

#[def_test]
fn rejects_deleted_descriptor_tag() {
    let superblock = journal_superblock(1, 0);
    let mut journal = TestJournal::new();
    journal.insert(1, one_tag_descriptor(600, JBD2_FLAG_DELETED as u16));

    assert_eq!(
        scan_journal(&superblock, &journal),
        Err(Ext4Error::Unsupported(
            UnsupportedKind::DeletedJournalUpdate
        ))
    );
}

#[def_test]
fn rejects_unknown_control_block_type_with_matching_sequence() {
    let superblock = journal_superblock(1, 0);
    let mut journal = TestJournal::new();
    journal.insert(1, block_header(99, SEQUENCE));

    assert_eq!(
        scan_journal(&superblock, &journal),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal))
    );
}

#[def_test]
fn scan_limit_reports_journal_too_large() {
    assert_eq!(
        ensure_scan_limit(MAX_SCANNED_UPDATES, 1, MAX_SCANNED_UPDATES),
        Err(Ext4Error::Unsupported(UnsupportedKind::JournalTooLarge))
    );
}

fn journal_superblock(start: u32, incompat: u32) -> JournalSuperblock {
    journal_superblock_with_sequence(start, incompat, SEQUENCE)
}

fn journal_superblock_with_sequence(
    start: u32,
    incompat: u32,
    sequence: u32,
) -> JournalSuperblock {
    let mut bytes = [0; JOURNAL_SUPERBLOCK_SIZE];
    put_be_u32(&mut bytes, 0, JBD2_MAGIC_NUMBER);
    put_be_u32(&mut bytes, 4, 4);
    put_be_u32(&mut bytes, 12, BLOCK_SIZE as u32);
    put_be_u32(&mut bytes, 16, 1024);
    put_be_u32(&mut bytes, 20, 1);
    put_be_u32(&mut bytes, 24, sequence);
    put_be_u32(&mut bytes, 28, start);
    put_be_u32(&mut bytes, 40, incompat);
    bytes[48..64].copy_from_slice(&UUID);
    put_be_u32(&mut bytes, 64, 1);
    if incompat & (FEATURE_INCOMPAT_CSUM_V2 | FEATURE_INCOMPAT_CSUM_V3) != 0 {
        bytes[0x50] = 4;
        let checksum = checksum_with_zeroed_u32(u32::MAX, &bytes, 0xfc).unwrap();
        put_be_u32(&mut bytes, 0xfc, checksum);
    }
    JournalSuperblock::decode(&bytes, BLOCK_SIZE as u32, 1024, UUID).unwrap()
}

fn block_header(block_type: u32, sequence: u32) -> Vec<u8> {
    let mut bytes = vec![0; BLOCK_SIZE];
    put_be_u32(&mut bytes, 0, JBD2_MAGIC_NUMBER);
    put_be_u32(&mut bytes, 4, block_type);
    put_be_u32(&mut bytes, 8, sequence);
    bytes
}

fn one_tag_descriptor(target: u32, additional_flags: u16) -> Vec<u8> {
    one_tag_descriptor_with_sequence(target, additional_flags, SEQUENCE)
}

fn one_tag_descriptor_with_sequence(
    target: u32,
    additional_flags: u16,
    sequence: u32,
) -> Vec<u8> {
    let mut descriptor = block_header(JBD2_DESCRIPTOR_BLOCK, sequence);
    put_be_u32(&mut descriptor, 12, target);
    put_be_u16(
        &mut descriptor,
        18,
        additional_flags | JBD2_FLAG_LAST_TAG as u16,
    );
    if additional_flags & JBD2_FLAG_SAME_UUID as u16 == 0 {
        descriptor[20..36].copy_from_slice(&UUID);
    }
    descriptor
}

fn v3_descriptor_with_one_tag(
    target: u32,
    sequence: u32,
    checksum_seed: u32,
    data: &[u8],
) -> Vec<u8> {
    let mut descriptor = block_header(JBD2_DESCRIPTOR_BLOCK, sequence);
    put_be_u32(&mut descriptor, 12, target);
    put_be_u32(&mut descriptor, 16, JBD2_FLAG_LAST_TAG);
    let data_checksum = checksum::crc32c(checksum_seed, &sequence.to_be_bytes());
    let data_checksum = checksum::crc32c(data_checksum, data);
    put_be_u32(&mut descriptor, 24, data_checksum);
    set_block_tail_checksum(&mut descriptor, checksum_seed);
    descriptor
}

fn v3_commit(sequence: u32, checksum_seed: u32) -> Vec<u8> {
    checksummed_commit(sequence, checksum_seed)
}

fn checksummed_commit(sequence: u32, checksum_seed: u32) -> Vec<u8> {
    let mut commit = block_header(JBD2_COMMIT_BLOCK, sequence);
    let commit_checksum = checksum_with_zeroed_u32(checksum_seed, &commit, 16).unwrap();
    put_be_u32(&mut commit, 16, commit_checksum);
    commit
}

fn set_block_tail_checksum(bytes: &mut [u8], seed: u32) {
    let offset = bytes.len() - JBD2_BLOCK_TAIL_SIZE;
    let checksum = checksum_with_zeroed_u32(seed, bytes, offset).unwrap();
    put_be_u32(bytes, offset, checksum);
}

fn put_v2_32bit_tag(
    bytes: &mut [u8],
    offset: usize,
    target: u32,
    flags: u16,
    checksum_seed: u32,
    data: &[u8],
) {
    put_be_u32(bytes, offset, target);
    let checksum = checksum::crc32c(checksum_seed, &SEQUENCE.to_be_bytes());
    let checksum = checksum::crc32c(checksum, data);
    put_be_u16(bytes, offset + 4, checksum as u16);
    put_be_u16(bytes, offset + 6, flags);
}

fn put_v2_64bit_tag(
    bytes: &mut [u8],
    offset: usize,
    target: u64,
    flags: u16,
    checksum_seed: u32,
    data: &[u8],
) {
    put_be_u32(bytes, offset, target as u32);
    let checksum = checksum::crc32c(checksum_seed, &SEQUENCE.to_be_bytes());
    let checksum = checksum::crc32c(checksum, data);
    put_be_u16(bytes, offset + 4, checksum as u16);
    put_be_u16(bytes, offset + 6, flags);
    put_be_u32(bytes, offset + 8, (target >> 32) as u32);
}

fn put_be_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_be_bytes());
}

fn put_be_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
}

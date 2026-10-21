// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::collections::BTreeMap;
use core::cell::RefCell;

use block::DriverError;
use unittest::def_test;

use super::*;
use crate::jbd2::{
    JournalBlockReader, JournalCredits, JournalReplayBlockWriter, JournalSuperblock,
    JournalTransactions, replay_scanned_journal, scan_journal,
    superblock::JOURNAL_SUPERBLOCK_SIZE,
};

const UUID: [u8; 16] = [0x5a; 16];
const BLOCK_SIZE: usize = 1024;
const JOURNAL_BLOCKS: u32 = 1024;
const SEQUENCE: u32 = 42;

struct TestJournal {
    blocks: RefCell<BTreeMap<u32, Vec<u8>>>,
    write_order: RefCell<Vec<JournalBlock>>,
    write_request_block_counts: RefCell<Vec<u32>>,
    read_request_count: RefCell<usize>,
    fail_write_at: RefCell<Option<usize>>,
    flush_count: RefCell<usize>,
}

struct TestReplayWriter {
    blocks: RefCell<BTreeMap<u64, Vec<u8>>>,
}

impl TestJournal {
    fn new() -> Self {
        Self {
            blocks: RefCell::new(BTreeMap::new()),
            write_order: RefCell::new(Vec::new()),
            write_request_block_counts: RefCell::new(Vec::new()),
            read_request_count: RefCell::new(0),
            fail_write_at: RefCell::new(None),
            flush_count: RefCell::new(0),
        }
    }

    fn with_superblock(superblock: &JournalSuperblock) -> Self {
        let journal = Self::new();
        journal.insert_superblock(superblock);
        journal
    }

    fn insert_superblock(&self, superblock: &JournalSuperblock) {
        let block = superblock.encoded().to_vec();
        self.blocks.borrow_mut().insert(0, block);
    }

    fn fail_write_at(&self, write_index: usize) {
        *self.fail_write_at.borrow_mut() = Some(write_index);
    }

    fn write_one(&self, block: JournalBlock, input: &[u8]) -> Ext4Result<()> {
        if input.len() != BLOCK_SIZE {
            return Err(Ext4Error::InvalidBufferLength {
                expected: BLOCK_SIZE,
                actual: input.len(),
            });
        }
        if *self.fail_write_at.borrow() == Some(self.write_order.borrow().len()) {
            return Err(Ext4Error::Device(DriverError::Io));
        }
        self.write_order.borrow_mut().push(block);
        self.blocks.borrow_mut().insert(block.get(), input.to_vec());
        Ok(())
    }
}

impl JournalBlockWriter for TestJournal {
    fn write_journal_block(&self, block: JournalBlock, input: &[u8]) -> Ext4Result<()> {
        self.write_request_block_counts.borrow_mut().push(1);
        self.write_one(block, input)
    }

    fn write_journal_blocks(
        &self,
        start: JournalBlock,
        block_count: u32,
        input: &[u8],
    ) -> Ext4Result<()> {
        let expected = usize::try_from(block_count)
            .map_err(|_| Ext4Error::Overflow)?
            .checked_mul(BLOCK_SIZE)
            .ok_or(Ext4Error::Overflow)?;
        if input.len() != expected {
            return Err(Ext4Error::InvalidBufferLength {
                expected,
                actual: input.len(),
            });
        }
        self.write_request_block_counts
            .borrow_mut()
            .push(block_count);
        for index in 0..block_count {
            let block = start
                .get()
                .checked_add(index)
                .map(JournalBlock::new)
                .ok_or(Ext4Error::Overflow)?;
            let offset = usize::try_from(index)
                .map_err(|_| Ext4Error::Overflow)?
                .checked_mul(BLOCK_SIZE)
                .ok_or(Ext4Error::Overflow)?;
            let end = offset.checked_add(BLOCK_SIZE).ok_or(Ext4Error::Overflow)?;
            self.write_one(block, input.get(offset..end).ok_or(Ext4Error::OutOfBounds)?)?;
        }
        Ok(())
    }

    fn flush_journal(&self) -> Ext4Result<()> {
        *self.flush_count.borrow_mut() += 1;
        Ok(())
    }
}

impl JournalBlockReader for TestJournal {
    fn read_journal_block(&self, block: JournalBlock, output: &mut [u8]) -> Ext4Result<()> {
        *self.read_request_count.borrow_mut() += 1;
        if output.len() != BLOCK_SIZE {
            return Err(Ext4Error::InvalidBufferLength {
                expected: BLOCK_SIZE,
                actual: output.len(),
            });
        }
        if let Some(bytes) = self.blocks.borrow().get(&block.get()) {
            output.copy_from_slice(bytes);
        } else {
            output.fill(0);
        }
        Ok(())
    }
}

impl TestReplayWriter {
    fn new() -> Self {
        Self {
            blocks: RefCell::new(BTreeMap::new()),
        }
    }
}

impl JournalReplayBlockWriter for TestReplayWriter {
    fn write_replay_block(&self, block: JournalTargetBlock, input: &[u8]) -> Ext4Result<()> {
        self.blocks.borrow_mut().insert(block.get(), input.to_vec());
        Ok(())
    }

    fn flush_replay(&self) -> Ext4Result<()> {
        Ok(())
    }
}

#[def_test]
fn writes_scannable_descriptor_data_and_commit_blocks() {
    let superblock = journal_superblock(1, 0, SEQUENCE);
    let journal = TestJournal::new();
    let (commit, blocks) = committed_blocks(&[
        (FilesystemBlock::new(100), vec![0x11; BLOCK_SIZE]),
        (FilesystemBlock::new(200), vec![0x22; BLOCK_SIZE]),
    ]);

    let report = write_journal_commit(
        &superblock,
        &journal,
        JournalBlock::new(1),
        &commit,
        &blocks,
    )
    .unwrap();

    assert_eq!(report.transaction(), TransactionId::new(SEQUENCE));
    assert_eq!(report.start(), JournalBlock::new(1));
    assert_eq!(report.head(), JournalBlock::new(5));
    assert_eq!(report.next_sequence(), TransactionId::new(SEQUENCE + 1));
    assert_eq!(report.update_count(), 2);
    assert_eq!(report.journal_block_count(), 4);
    assert_eq!(*journal.flush_count.borrow(), 1);
    assert_eq!(journal.write_request_block_counts.borrow().as_slice(), &[4]);

    let scan = scan_journal(&superblock, &journal).unwrap();
    assert_eq!(scan.transactions().len(), 1);
    let transaction = &scan.transactions()[0];
    assert_eq!(
        transaction
            .updates()
            .iter()
            .map(|update| update.target())
            .collect::<Vec<_>>(),
        vec![JournalTargetBlock::new(100), JournalTargetBlock::new(200)]
    );
    assert_eq!(
        transaction
            .updates()
            .iter()
            .map(|update| update.log_block())
            .collect::<Vec<_>>(),
        vec![JournalBlock::new(2), JournalBlock::new(3)]
    );
}

#[def_test]
fn bounds_journal_write_batches() {
    let superblock = journal_superblock(1, 0, SEQUENCE);
    let journal = TestJournal::new();
    let input = (0..140)
        .map(|index| {
            (
                FilesystemBlock::new(1_000 + index),
                vec![index as u8; BLOCK_SIZE],
            )
        })
        .collect::<Vec<_>>();
    let (commit, blocks) = committed_blocks(&input);

    let report = write_journal_commit(
        &superblock,
        &journal,
        JournalBlock::new(1),
        &commit,
        &blocks,
    )
    .unwrap();

    let request_blocks = journal.write_request_block_counts.borrow();
    assert!(request_blocks.len() > 1);
    assert!(
        request_blocks
            .iter()
            .all(|count| *count as usize * BLOCK_SIZE <= MAX_JOURNAL_WRITE_BYTES)
    );
    assert_eq!(
        request_blocks.iter().copied().sum::<u32>(),
        report.journal_block_count()
    );
}

#[def_test]
fn writes_v3_checksummed_transaction_that_replay_can_apply() {
    let superblock = journal_superblock(1, FEATURE_INCOMPAT_CSUM_V3, SEQUENCE);
    let journal = TestJournal::new();
    let (commit, blocks) = committed_blocks(&[
        (FilesystemBlock::new(300), vec![0x33; BLOCK_SIZE]),
        (FilesystemBlock::new(400), vec![0x44; BLOCK_SIZE]),
    ]);

    write_journal_commit(
        &superblock,
        &journal,
        JournalBlock::new(1),
        &commit,
        &blocks,
    )
    .unwrap();

    let scan = scan_journal(&superblock, &journal).unwrap();
    let replay = TestReplayWriter::new();
    replay_scanned_journal(&journal, &replay, &scan, BLOCK_SIZE).unwrap();
    assert_eq!(replay.blocks.borrow().get(&300).unwrap()[0], 0x33);
    assert_eq!(replay.blocks.borrow().get(&400).unwrap()[0], 0x44);
}

#[def_test]
fn writes_online_revoke_block_that_suppresses_older_replay() {
    let superblock = journal_superblock(
        1,
        FEATURE_INCOMPAT_REVOKE | FEATURE_INCOMPAT_CSUM_V3,
        SEQUENCE,
    );
    let journal = TestJournal::new();
    let (first_commit, first_blocks) =
        committed_blocks(&[(FilesystemBlock::new(300), vec![0x33; BLOCK_SIZE])]);
    let first = write_journal_commit(
        &superblock,
        &journal,
        JournalBlock::new(1),
        &first_commit,
        &first_blocks,
    )
    .unwrap();
    let revoke_commit =
        committed_revokes_with_sequence(SEQUENCE.wrapping_add(1), &[FilesystemBlock::new(300)]);

    let second =
        write_journal_commit(&superblock, &journal, first.head(), &revoke_commit, &[]).unwrap();

    assert_eq!(second.update_count(), 0);
    assert_eq!(second.revoke_count(), 1);
    assert_eq!(second.journal_block_count(), 2);
    let scan = scan_journal(&superblock, &journal).unwrap();
    assert_eq!(scan.transactions().len(), 2);
    assert_eq!(
        scan.transactions()[1].revoked_blocks(),
        &[JournalTargetBlock::new(300)]
    );
    let replay = TestReplayWriter::new();
    let report = replay_scanned_journal(&journal, &replay, &scan, BLOCK_SIZE)
        .unwrap()
        .into_report();
    assert_eq!(report.update_count(), 0);
    assert_eq!(report.revoke_hit_count(), 1);
    assert!(!replay.blocks.borrow().contains_key(&300));
}

#[def_test]
fn refuses_to_write_revoke_block_without_revoke_feature() {
    let superblock = journal_superblock(1, FEATURE_INCOMPAT_CSUM_V3, SEQUENCE);
    let journal = TestJournal::new();
    let revoke_commit = committed_revokes_with_sequence(SEQUENCE, &[FilesystemBlock::new(300)]);

    assert_eq!(
        write_journal_commit(
            &superblock,
            &journal,
            JournalBlock::new(1),
            &revoke_commit,
            &[]
        ),
        Err(Ext4Error::InvalidJournalTransaction)
    );
    assert_eq!(*journal.flush_count.borrow(), 0);
    assert!(!journal.blocks.borrow().values().any(|block| {
        block
            .get(4..8)
            .is_some_and(|block_type| block_type == JBD2_REVOKE_BLOCK.to_be_bytes())
    }));
}

#[def_test]
fn writes_64bit_revoke_entries_without_truncating_block_numbers() {
    let superblock = journal_superblock(
        1,
        FEATURE_INCOMPAT_REVOKE | FEATURE_INCOMPAT_64BIT,
        SEQUENCE,
    );
    let journal = TestJournal::new();
    let block = FilesystemBlock::new(0x0000_0001_1234_5678);
    let commit = committed_revokes_with_sequence(SEQUENCE, &[block]);

    let report =
        write_journal_commit(&superblock, &journal, JournalBlock::new(1), &commit, &[])
            .unwrap();

    assert_eq!(report.revoke_count(), 1);
    let scan = scan_journal(&superblock, &journal).unwrap();
    assert_eq!(
        scan.transactions()[0].revoked_blocks(),
        &[JournalTargetBlock::new(0x0000_0001_1234_5678)]
    );
}

#[def_test]
fn splits_revoke_records_across_multiple_revoke_blocks() {
    let superblock = journal_superblock(1, FEATURE_INCOMPAT_REVOKE, SEQUENCE);
    let journal = TestJournal::new();
    let revoked = (0..253)
        .map(|index| FilesystemBlock::new(1_000 + index))
        .collect::<Vec<_>>();
    let commit = committed_revokes_with_sequence(SEQUENCE, &revoked);

    let report =
        write_journal_commit(&superblock, &journal, JournalBlock::new(1), &commit, &[])
            .unwrap();

    assert_eq!(report.revoke_count(), 253);
    assert_eq!(report.journal_block_count(), 3);
    let scan = scan_journal(&superblock, &journal).unwrap();
    assert_eq!(scan.transactions()[0].revoked_blocks().len(), 253);
    assert_eq!(
        scan.transactions()[0].revoked_blocks()[252],
        JournalTargetBlock::new(1_252)
    );
}

#[def_test]
fn escapes_jbd2_magic_in_logged_data_and_replay_restores_it() {
    let superblock = journal_superblock(1, FEATURE_INCOMPAT_CSUM_V3, SEQUENCE);
    let journal = TestJournal::new();
    let mut bytes = vec![0x77; BLOCK_SIZE];
    bytes[..4].copy_from_slice(&JBD2_MAGIC_NUMBER.to_be_bytes());
    let (commit, blocks) = committed_blocks(&[(FilesystemBlock::new(500), bytes)]);

    write_journal_commit(
        &superblock,
        &journal,
        JournalBlock::new(1),
        &commit,
        &blocks,
    )
    .unwrap();

    assert_eq!(
        &journal.blocks.borrow().get(&2).unwrap()[..4],
        &[0, 0, 0, 0]
    );
    let scan = scan_journal(&superblock, &journal).unwrap();
    assert!(scan.transactions()[0].updates()[0].is_escaped());
    let replay = TestReplayWriter::new();
    replay_scanned_journal(&journal, &replay, &scan, BLOCK_SIZE).unwrap();
    assert_eq!(
        &replay.blocks.borrow().get(&500).unwrap()[..4],
        &JBD2_MAGIC_NUMBER.to_be_bytes()
    );
}

#[def_test]
fn wraps_written_transaction_across_log_end() {
    let superblock = journal_superblock(1023, 0, SEQUENCE);
    let journal = TestJournal::new();
    let (commit, blocks) =
        committed_blocks(&[(FilesystemBlock::new(600), vec![0x66; BLOCK_SIZE])]);

    let report = write_journal_commit(
        &superblock,
        &journal,
        JournalBlock::new(1023),
        &commit,
        &blocks,
    )
    .unwrap();

    assert_eq!(report.head(), JournalBlock::new(3));
    assert_eq!(
        journal.write_request_block_counts.borrow().as_slice(),
        &[1, 2]
    );
    assert!(journal.blocks.borrow().contains_key(&1023));
    assert!(journal.blocks.borrow().contains_key(&1));
    assert!(journal.blocks.borrow().contains_key(&2));
    let scan = scan_journal(&superblock, &journal).unwrap();
    assert_eq!(
        scan.transactions()[0].updates()[0].log_block(),
        JournalBlock::new(1)
    );
}

#[def_test]
fn rejects_blocks_that_do_not_match_commit_record() {
    let superblock = journal_superblock(1, 0, SEQUENCE);
    let journal = TestJournal::new();
    let (commit, mut blocks) =
        committed_blocks(&[(FilesystemBlock::new(700), vec![0x77; BLOCK_SIZE])]);
    blocks[0] = JournalCommitBlock::new(
        FilesystemBlock::new(701),
        Arc::from(vec![0x77; BLOCK_SIZE].into_boxed_slice()),
    );

    assert_eq!(
        write_journal_commit(
            &superblock,
            &journal,
            JournalBlock::new(1),
            &commit,
            &blocks,
        ),
        Err(Ext4Error::InvalidJournalTransaction)
    );
}

#[def_test]
fn persisted_commit_marks_journal_active_and_recoverable() {
    let superblock = journal_superblock(0, 0, SEQUENCE);
    let journal = TestJournal::with_superblock(&superblock);
    let (commit, blocks) =
        committed_blocks(&[(FilesystemBlock::new(800), vec![0x88; BLOCK_SIZE])]);

    let (persisted, active_superblock) =
        persist_journal_commit(&superblock, &journal, None, &commit, &blocks).unwrap();

    assert_eq!(persisted.log().start(), JournalBlock::new(1));
    assert_eq!(persisted.log().head(), JournalBlock::new(4));
    assert_eq!(
        active_superblock.start(),
        JournalStart::Block(JournalBlock::new(1))
    );
    assert_eq!(active_superblock.head(), JournalBlock::new(1));
    assert_eq!(*journal.flush_count.borrow(), 2);
    assert_eq!(
        journal.write_request_block_counts.borrow().as_slice(),
        &[1, 3]
    );
    assert_eq!(*journal.read_request_count.borrow(), 0);
    assert_eq!(
        journal.write_order.borrow().first().copied(),
        Some(JournalBlock::new(0))
    );

    let scan = scan_journal(&active_superblock, &journal).unwrap();
    assert_eq!(scan.transactions().len(), 1);
    let replay = TestReplayWriter::new();
    replay_scanned_journal(&journal, &replay, &scan, BLOCK_SIZE).unwrap();
    assert_eq!(replay.blocks.borrow().get(&800).unwrap()[0], 0x88);
}

#[def_test]
fn power_loss_after_first_activation_does_not_look_like_a_clean_journal() {
    let superblock = journal_superblock(0, 0, SEQUENCE);
    let journal = TestJournal::with_superblock(&superblock);
    let (commit, blocks) =
        committed_blocks(&[(FilesystemBlock::new(800), vec![0x88; BLOCK_SIZE])]);
    // Write 0 activates and flushes the journal; write 1 is the first
    // descriptor block and models power loss in the reviewed window.
    journal.fail_write_at(1);
    assert!(matches!(
        persist_journal_commit(&superblock, &journal, None, commit.as_ref(), &blocks),
        Err(Ext4Error::Device(DriverError::Io))
    ));

    let mut bytes = vec![0; BLOCK_SIZE];
    journal
        .read_journal_block(JournalBlock::new(0), &mut bytes)
        .unwrap();
    let active =
        JournalSuperblock::decode(&bytes, BLOCK_SIZE as u32, JOURNAL_BLOCKS, UUID).unwrap();
    let start = clean_log_head(&superblock);
    assert_eq!(active.start(), JournalStart::Block(start));
    assert_eq!(*journal.flush_count.borrow(), 1);
    assert_eq!(
        journal.write_order.borrow().as_slice(),
        &[JournalBlock::new(0)]
    );

    let scan = scan_journal(&active, &journal).unwrap();
    assert!(scan.transactions().is_empty());
    assert_eq!(scan.next_sequence(), TransactionId::new(SEQUENCE));
}

#[def_test]
fn checkpoint_finish_marks_journal_empty_after_persisted_commit() {
    let superblock = journal_superblock(0, FEATURE_INCOMPAT_CSUM_V3, SEQUENCE);
    let journal = TestJournal::with_superblock(&superblock);
    let (commit, blocks) =
        committed_blocks(&[(FilesystemBlock::new(900), vec![0x99; BLOCK_SIZE])]);

    let (persisted, active_superblock) =
        persist_journal_commit(&superblock, &journal, None, &commit, &blocks).unwrap();
    let clean =
        finish_journal_checkpoint_batch(&active_superblock, &journal, &[persisted], None)
            .unwrap();

    assert_eq!(clean.start(), JournalStart::Zero);
    assert_eq!(clean.sequence(), TransactionId::new(SEQUENCE + 1));
    assert_eq!(clean.head(), persisted.log().head());
    assert_eq!(*journal.flush_count.borrow(), 3);
    let scan = scan_journal(&clean, &journal).unwrap();
    assert!(scan.transactions().is_empty());
}

#[def_test]
fn persist_commit_rejects_active_journal_without_latest_commit() {
    let superblock = journal_superblock(1, 0, SEQUENCE);
    let journal = TestJournal::with_superblock(&superblock);
    let (commit, blocks) =
        committed_blocks(&[(FilesystemBlock::new(1000), vec![0xaa; BLOCK_SIZE])]);

    assert_eq!(
        persist_journal_commit(&superblock, &journal, None, &commit, &blocks).map(|_| ()),
        Err(Ext4Error::InvalidJournalTransaction)
    );
}

#[def_test]
fn append_space_keeps_one_block_between_head_and_tail() {
    let superblock = journal_superblock(1, 0, SEQUENCE);

    assert_eq!(
        ensure_log_space(
            &superblock,
            JournalBlock::new(1),
            JournalBlock::new(JOURNAL_BLOCKS - 2),
            2,
        ),
        Err(Ext4Error::JournalBusy)
    );
    ensure_log_space(
        &superblock,
        JournalBlock::new(1),
        JournalBlock::new(JOURNAL_BLOCKS - 2),
        1,
    )
    .unwrap();
}

#[def_test]
fn checkpoint_prefix_reserves_append_space_across_ring_wrap() {
    for head in [JOURNAL_BLOCKS - 11, JOURNAL_BLOCKS - 4] {
        let mut bytes = journal_superblock_bytes(0, 0, SEQUENCE);
        put_be_u32(&mut bytes, 20, JOURNAL_BLOCKS - 11).unwrap();
        put_be_u32(&mut bytes, 0x58, head).unwrap();
        let mut superblock =
            JournalSuperblock::decode(&bytes, BLOCK_SIZE as u32, JOURNAL_BLOCKS, UUID).unwrap();
        let journal = TestJournal::with_superblock(&superblock);
        let mut pending = Vec::new();
        for offset in 0..3 {
            let (commit, blocks) = committed_blocks_with_sequence(
                SEQUENCE + offset,
                &[(FilesystemBlock::new(1100), vec![offset as u8; BLOCK_SIZE])],
            );
            let (persisted, active) =
                persist_journal_commit(&superblock, &journal, pending.last(), &commit, &blocks)
                    .unwrap();
            pending.push(persisted);
            superblock = active;
        }
        // Reclaiming three blocks leaves five free; equality still requires
        // a second transaction because an append must retain one spare.
        assert_eq!(
            journal_checkpoint_prefix_len(&superblock, pending.iter(), 4),
            Ok(1)
        );
        assert_eq!(
            journal_checkpoint_prefix_len(&superblock, pending.iter(), 5),
            Ok(2)
        );
        assert_eq!(
            journal_checkpoint_prefix_len(&superblock, pending.iter(), 8),
            Ok(3)
        );
        assert_eq!(
            journal_checkpoint_prefix_len(&superblock, pending.iter(), 11),
            Err(Ext4Error::Unsupported(UnsupportedKind::JournalTooLarge))
        );
    }
}

#[def_test]
fn appends_commits_and_advances_checkpoint_tail_in_order() {
    let superblock = journal_superblock(0, FEATURE_INCOMPAT_CSUM_V3, SEQUENCE);
    let journal = TestJournal::with_superblock(&superblock);
    let (first_commit, first_blocks) =
        committed_blocks(&[(FilesystemBlock::new(1100), vec![0xab; BLOCK_SIZE])]);
    let (second_commit, second_blocks) = committed_blocks_with_sequence(
        SEQUENCE.wrapping_add(1),
        &[(FilesystemBlock::new(1200), vec![0xbc; BLOCK_SIZE])],
    );

    let (first, first_superblock) =
        persist_journal_commit(&superblock, &journal, None, &first_commit, &first_blocks)
            .unwrap();
    let (second, second_superblock) = persist_journal_commit(
        &first_superblock,
        &journal,
        Some(&first),
        &second_commit,
        &second_blocks,
    )
    .unwrap();

    assert_eq!(second_superblock.sequence(), first.log().transaction());
    assert_eq!(
        second_superblock.start(),
        JournalStart::Block(first.log().start())
    );
    assert_eq!(second_superblock.head(), JournalBlock::new(1));
    let scan = scan_journal(&second_superblock, &journal).unwrap();
    assert_eq!(scan.transactions().len(), 2);

    let flushes_before = *journal.flush_count.borrow();
    let writes_before = journal.write_order.borrow().len();
    assert_eq!(
        finish_journal_checkpoint_batch(&second_superblock, &journal, &[first, first], None),
        Err(Ext4Error::InvalidJournalTransaction)
    );
    assert_eq!(*journal.flush_count.borrow(), flushes_before);
    assert_eq!(journal.write_order.borrow().len(), writes_before);
    assert_eq!(
        scan_journal(&second_superblock, &journal)
            .unwrap()
            .transactions()
            .len(),
        2
    );

    let second_is_oldest =
        finish_journal_checkpoint_batch(&second_superblock, &journal, &[first], Some(&second))
            .unwrap();
    assert_eq!(second_is_oldest.sequence(), second.log().transaction());
    assert_eq!(
        second_is_oldest.start(),
        JournalStart::Block(second.log().start())
    );
    assert_eq!(second_is_oldest.head(), JournalBlock::new(1));
    let scan = scan_journal(&second_is_oldest, &journal).unwrap();
    assert_eq!(scan.transactions().len(), 1);
    assert_eq!(
        scan.transactions()[0].sequence(),
        second.log().transaction()
    );

    let clean =
        finish_journal_checkpoint_batch(&second_is_oldest, &journal, &[second], None).unwrap();
    assert_eq!(clean.start(), JournalStart::Zero);
    assert_eq!(clean.sequence(), second.log().next_sequence());
    assert_eq!(clean.head(), second.log().head());
}

fn committed_blocks(
    input: &[(FilesystemBlock, Vec<u8>)],
) -> (Arc<RuntimeTransaction>, Vec<JournalCommitBlock>) {
    committed_blocks_with_sequence(SEQUENCE, input)
}

fn committed_blocks_with_sequence(
    sequence: u32,
    input: &[(FilesystemBlock, Vec<u8>)],
) -> (Arc<RuntimeTransaction>, Vec<JournalCommitBlock>) {
    let journal = JournalTransactions::new(TransactionId::new(sequence));
    let mut handle = journal
        .begin(JournalCredits::new(input.len() as u32))
        .unwrap();
    for (block, _) in input {
        handle.consume_metadata_credit(*block).unwrap();
    }
    let transaction = handle.id();
    drop(handle);
    let commit = journal.force_commit(transaction).unwrap();
    let blocks = input
        .iter()
        .map(|(target, bytes)| {
            JournalCommitBlock::new(*target, Arc::from(bytes.clone().into_boxed_slice()))
        })
        .collect();
    (commit, blocks)
}

fn committed_revokes_with_sequence(
    sequence: u32,
    revoked: &[FilesystemBlock],
) -> Arc<RuntimeTransaction> {
    let journal = JournalTransactions::new(TransactionId::new(sequence));
    let mut handle = journal
        .begin(JournalCredits::new(revoked.len() as u32))
        .unwrap();
    for block in revoked {
        handle.revoke_metadata_block(*block).unwrap();
    }
    let transaction = handle.id();
    drop(handle);
    journal.force_commit(transaction).unwrap()
}

fn journal_superblock(start: u32, incompat: u32, sequence: u32) -> JournalSuperblock {
    let bytes = journal_superblock_bytes(start, incompat, sequence);
    JournalSuperblock::decode(&bytes, BLOCK_SIZE as u32, JOURNAL_BLOCKS, UUID).unwrap()
}

fn journal_superblock_bytes(
    start: u32,
    incompat: u32,
    sequence: u32,
) -> [u8; JOURNAL_SUPERBLOCK_SIZE] {
    let mut bytes = [0; JOURNAL_SUPERBLOCK_SIZE];
    put_be_u32(&mut bytes, 0, JBD2_MAGIC_NUMBER).unwrap();
    put_be_u32(&mut bytes, 4, 4).unwrap();
    put_be_u32(&mut bytes, 12, BLOCK_SIZE as u32).unwrap();
    put_be_u32(&mut bytes, 16, JOURNAL_BLOCKS).unwrap();
    put_be_u32(&mut bytes, 20, 1).unwrap();
    put_be_u32(&mut bytes, 24, sequence).unwrap();
    put_be_u32(&mut bytes, 28, start).unwrap();
    put_be_u32(&mut bytes, 0x58, if start == 0 { 1 } else { start }).unwrap();
    put_be_u32(&mut bytes, 40, incompat).unwrap();
    bytes[48..64].copy_from_slice(&UUID);
    put_be_u32(&mut bytes, 64, 1).unwrap();
    if incompat & (FEATURE_INCOMPAT_CSUM_V2 | FEATURE_INCOMPAT_CSUM_V3) != 0 {
        bytes[0x50] = 4;
        let checksum = checksum_with_zeroed_u32(u32::MAX, &bytes, 0xfc).unwrap();
        put_be_u32(&mut bytes, 0xfc, checksum).unwrap();
    }
    bytes
}

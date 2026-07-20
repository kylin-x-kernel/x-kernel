// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! JBD2 recovery replay planning.

#[cfg(test)]
use alloc::vec::Vec;
use alloc::{collections::BTreeMap, vec};

use crate::{
    Ext4Error, Ext4Result,
    jbd2::{
        JournalBlockReader, JournalLogScan, JournalTransactionState,
        mapper::{JournalBlock, JournalTargetBlock, TransactionId},
    },
};

const JBD2_MAGIC_NUMBER: u32 = 0xc03b_3998;

/// One metadata block write selected for journal replay.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct JournalReplayUpdate {
    transaction: TransactionId,
    target: JournalTargetBlock,
    log_block: JournalBlock,
    is_escaped: bool,
}

impl JournalReplayUpdate {
    /// Returns the committed transaction that owns this replay update.
    #[cfg(test)]
    const fn transaction(&self) -> TransactionId {
        self.transaction
    }

    /// Returns the filesystem block that replay should update.
    const fn target(&self) -> JournalTargetBlock {
        self.target
    }

    /// Returns the journal block containing the replacement contents.
    const fn log_block(&self) -> JournalBlock {
        self.log_block
    }

    /// Returns whether replay must restore the escaped JBD2 magic value.
    const fn is_escaped(&self) -> bool {
        self.is_escaped
    }
}

/// Writes metadata blocks selected by JBD2 recovery.
pub(crate) trait JournalReplayBlockWriter {
    /// Writes one complete filesystem block from the replay stream.
    fn write_replay_block(&self, block: JournalTargetBlock, input: &[u8]) -> Ext4Result<()>;

    /// Flushes replay writes to stable storage.
    fn flush_replay(&self) -> Ext4Result<()>;
}

/// Ordered metadata writes required to replay a scanned JBD2 log.
#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
struct JournalReplayPlan {
    updates: Vec<JournalReplayUpdate>,
    head: JournalBlock,
    next_sequence: TransactionId,
    revoke_hit_count: usize,
}

#[cfg(test)]
impl JournalReplayPlan {
    /// Returns metadata updates in replay order.
    fn updates(&self) -> &[JournalReplayUpdate] {
        &self.updates
    }

    /// Returns whether this plan contains no metadata writes.
    fn is_empty(&self) -> bool {
        self.updates.is_empty()
    }

    /// Returns the first journal block after the recoverable log contents.
    const fn head(&self) -> JournalBlock {
        self.head
    }

    /// Returns the sequence expected for the next transaction.
    const fn next_sequence(&self) -> TransactionId {
        self.next_sequence
    }

    /// Returns how many descriptor updates were suppressed by revoke records.
    const fn revoke_hit_count(&self) -> usize {
        self.revoke_hit_count
    }
}

/// Summary of an executed journal replay plan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JournalReplayReport {
    update_count: usize,
    revoke_hit_count: usize,
    head: JournalBlock,
    next_sequence: TransactionId,
}

impl JournalReplayReport {
    /// Returns how many metadata blocks were written by replay.
    pub const fn update_count(&self) -> usize {
        self.update_count
    }

    /// Returns how many descriptor updates were suppressed by revoke records.
    pub const fn revoke_hit_count(&self) -> usize {
        self.revoke_hit_count
    }

    /// Returns the first journal block after the recovered log contents.
    pub const fn head(&self) -> JournalBlock {
        self.head
    }

    /// Returns the sequence expected for the next transaction.
    pub const fn next_sequence(&self) -> TransactionId {
        self.next_sequence
    }
}

/// Evidence that committed journal metadata updates were replayed and flushed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct JournalReplayApplied {
    report: JournalReplayReport,
}

impl JournalReplayApplied {
    /// Returns the replay summary without consuming the evidence.
    pub(crate) const fn report(self) -> JournalReplayReport {
        self.report
    }

    /// Consumes this state and returns the replay summary.
    pub(crate) const fn into_report(self) -> JournalReplayReport {
        self.report
    }
}

/// Builds the ordered replay writes for a scanned JBD2 log.
///
/// Only committed transactions contribute updates. Revoke records from
/// committed transactions suppress updates in the same or earlier transaction,
/// while later transactions for the same filesystem block still replay. This
/// mirrors Linux JBD2 recovery's revoke test without requiring a writable block
/// device at this stage.
#[cfg(test)]
fn plan_journal_replay(scan: &JournalLogScan) -> JournalReplayPlan {
    let latest_revokes = collect_latest_revokes(scan);
    let mut updates = Vec::new();
    let mut revoke_hit_count = 0;

    for (ordinal, transaction) in committed_transactions(scan).enumerate() {
        for update in transaction.updates() {
            if latest_revokes
                .get(&update.target())
                .is_some_and(|revoke_ordinal| *revoke_ordinal >= ordinal)
            {
                revoke_hit_count += 1;
                continue;
            }
            updates.push(JournalReplayUpdate {
                transaction: transaction.sequence(),
                target: update.target(),
                log_block: update.log_block(),
                is_escaped: update.is_escaped(),
            });
        }
    }

    JournalReplayPlan {
        updates,
        head: scan.head(),
        next_sequence: scan.next_sequence(),
        revoke_hit_count,
    }
}

/// Executes a planned journal replay against a filesystem block writer.
///
/// The caller provides the validated filesystem block size so that every
/// planned update is read and written as one complete ext4 block.
#[cfg(test)]
fn replay_journal_plan(
    reader: &impl JournalBlockReader,
    writer: &impl JournalReplayBlockWriter,
    plan: &JournalReplayPlan,
    block_size: usize,
) -> Ext4Result<JournalReplayReport> {
    if block_size == 0 {
        return Err(Ext4Error::InvalidBufferLength {
            expected: 1,
            actual: 0,
        });
    }

    let mut block = vec![0; block_size];
    for update in plan.updates() {
        read_journal_replay_update(reader, update, &mut block)?;
        writer.write_replay_block(update.target(), &block)?;
    }
    writer.flush_replay()?;

    Ok(JournalReplayReport {
        update_count: plan.updates().len(),
        revoke_hit_count: plan.revoke_hit_count(),
        head: plan.head(),
        next_sequence: plan.next_sequence(),
    })
}

/// Replays committed metadata updates directly from a scanned JBD2 log.
///
/// This uses the committed-transaction and revoke ordering tested by the
/// materialized replay-plan helper, but it does not allocate a second vector of
/// replay updates. The returned state proves that replay writes have been
/// flushed before the caller marks the journal empty.
pub(crate) fn replay_scanned_journal(
    reader: &impl JournalBlockReader,
    writer: &impl JournalReplayBlockWriter,
    scan: &JournalLogScan,
    block_size: usize,
) -> Ext4Result<JournalReplayApplied> {
    if block_size == 0 {
        return Err(Ext4Error::InvalidBufferLength {
            expected: 1,
            actual: 0,
        });
    }

    let latest_revokes = collect_latest_revokes(scan);
    let mut block = vec![0; block_size];
    let mut update_count = 0usize;
    let mut revoke_hit_count = 0usize;
    for (ordinal, transaction) in committed_transactions(scan).enumerate() {
        for update in transaction.updates() {
            if latest_revokes
                .get(&update.target())
                .is_some_and(|revoke_ordinal| *revoke_ordinal >= ordinal)
            {
                revoke_hit_count = revoke_hit_count.checked_add(1).ok_or(Ext4Error::Overflow)?;
                continue;
            }

            let replay_update = JournalReplayUpdate {
                transaction: transaction.sequence(),
                target: update.target(),
                log_block: update.log_block(),
                is_escaped: update.is_escaped(),
            };
            read_journal_replay_update(reader, &replay_update, &mut block)?;
            writer.write_replay_block(replay_update.target(), &block)?;
            update_count = update_count.checked_add(1).ok_or(Ext4Error::Overflow)?;
        }
    }
    writer.flush_replay()?;

    Ok(JournalReplayApplied {
        report: JournalReplayReport {
            update_count,
            revoke_hit_count,
            head: scan.head(),
            next_sequence: scan.next_sequence(),
        },
    })
}

/// Reads the replacement contents for one replay update.
///
/// If the descriptor tag was escaped, the returned buffer has the JBD2 magic
/// value restored at its beginning, matching the contents that should be
/// written to the target filesystem block.
fn read_journal_replay_update(
    reader: &impl JournalBlockReader,
    update: &JournalReplayUpdate,
    output: &mut [u8],
) -> Ext4Result<()> {
    reader.read_journal_block(update.log_block(), output)?;
    if update.is_escaped() {
        let actual = output.len();
        let magic = output.get_mut(..4).ok_or(Ext4Error::InvalidBufferLength {
            expected: 4,
            actual,
        })?;
        magic.copy_from_slice(&JBD2_MAGIC_NUMBER.to_be_bytes());
    }
    Ok(())
}

fn collect_latest_revokes(scan: &JournalLogScan) -> BTreeMap<JournalTargetBlock, usize> {
    let mut latest_revokes = BTreeMap::new();
    for (ordinal, transaction) in committed_transactions(scan).enumerate() {
        for block in transaction.revoked_blocks() {
            latest_revokes.insert(*block, ordinal);
        }
    }
    latest_revokes
}

fn committed_transactions(
    scan: &JournalLogScan,
) -> impl Iterator<Item = &crate::jbd2::JournalTransaction> {
    scan.transactions()
        .iter()
        .filter(|transaction| transaction.state() == JournalTransactionState::Committed)
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::BTreeMap};

    use super::*;
    use crate::{
        error::{Ext4Error, Ext4Result},
        jbd2::{JournalBlockReader, JournalSuperblock, scan_journal},
    };

    const JBD2_DESCRIPTOR_BLOCK: u32 = 1;
    const JBD2_COMMIT_BLOCK: u32 = 2;
    const JBD2_REVOKE_BLOCK: u32 = 5;
    const JBD2_FEATURE_INCOMPAT_REVOKE: u32 = 0x0000_0001;
    const JBD2_FLAG_ESCAPE: u16 = 1;
    const JBD2_FLAG_SAME_UUID: u16 = 2;
    const JBD2_FLAG_LAST_TAG: u16 = 8;
    const UUID: [u8; 16] = [0xa5; 16];
    const BLOCK_SIZE: usize = 1024;
    const JOURNAL_BLOCKS: u32 = 1024;
    const SEQUENCE: u32 = u32::MAX - 1;

    struct TestJournal {
        blocks: BTreeMap<u32, Vec<u8>>,
    }

    struct TestWriter {
        blocks: RefCell<BTreeMap<u64, Vec<u8>>>,
        is_flushed: RefCell<bool>,
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

    impl TestWriter {
        fn new() -> Self {
            Self {
                blocks: RefCell::new(BTreeMap::new()),
                is_flushed: RefCell::new(false),
            }
        }
    }

    impl JournalReplayBlockWriter for TestWriter {
        fn write_replay_block(&self, block: JournalTargetBlock, input: &[u8]) -> Ext4Result<()> {
            self.blocks.borrow_mut().insert(block.get(), input.to_vec());
            Ok(())
        }

        fn flush_replay(&self) -> Ext4Result<()> {
            *self.is_flushed.borrow_mut() = true;
            Ok(())
        }
    }

    #[test]
    fn plans_committed_updates_in_log_order() {
        let superblock = journal_superblock(1);
        let mut journal = TestJournal::new();
        journal.insert(1, descriptor_with_tag(100, 0, SEQUENCE));
        journal.insert(2, vec![0x11; BLOCK_SIZE]);
        journal.insert(3, block_header(JBD2_COMMIT_BLOCK, SEQUENCE));
        journal.insert(
            4,
            descriptor_with_tag(200, JBD2_FLAG_ESCAPE, SEQUENCE.wrapping_add(1)),
        );
        journal.insert(5, vec![0x22; BLOCK_SIZE]);
        journal.insert(6, block_header(JBD2_COMMIT_BLOCK, SEQUENCE.wrapping_add(1)));
        journal.insert(7, vec![0; BLOCK_SIZE]);

        let scan = scan_journal(&superblock, &journal).unwrap();
        let plan = plan_journal_replay(&scan);

        assert_eq!(plan.head(), JournalBlock::new(7));
        assert_eq!(
            plan.next_sequence(),
            TransactionId::new(SEQUENCE.wrapping_add(2))
        );
        assert_eq!(plan.revoke_hit_count(), 0);
        assert_eq!(
            plan.updates(),
            &[
                JournalReplayUpdate {
                    transaction: TransactionId::new(SEQUENCE),
                    target: JournalTargetBlock::new(100),
                    log_block: JournalBlock::new(2),
                    is_escaped: false,
                },
                JournalReplayUpdate {
                    transaction: TransactionId::new(SEQUENCE.wrapping_add(1)),
                    target: JournalTargetBlock::new(200),
                    log_block: JournalBlock::new(5),
                    is_escaped: true,
                },
            ]
        );
    }

    #[test]
    fn ignores_uncommitted_transaction_updates_and_revokes() {
        let superblock = journal_superblock(1);
        let mut journal = TestJournal::new();
        journal.insert(1, descriptor_with_tag(100, 0, SEQUENCE));
        journal.insert(2, vec![0x11; BLOCK_SIZE]);
        journal.insert(3, block_header(JBD2_COMMIT_BLOCK, SEQUENCE));
        journal.insert(4, revoke_with_block(100, SEQUENCE.wrapping_add(1)));
        journal.insert(5, vec![0; BLOCK_SIZE]);

        let scan = scan_journal(&superblock, &journal).unwrap();
        let plan = plan_journal_replay(&scan);

        assert_eq!(plan.revoke_hit_count(), 0);
        assert_eq!(plan.updates().len(), 1);
        assert_eq!(plan.updates()[0].target(), JournalTargetBlock::new(100));
    }

    #[test]
    fn committed_revoke_suppresses_same_or_earlier_transaction_update() {
        let superblock = journal_superblock(1);
        let mut journal = TestJournal::new();
        journal.insert(1, descriptor_with_tag(100, 0, SEQUENCE));
        journal.insert(2, vec![0x11; BLOCK_SIZE]);
        journal.insert(3, block_header(JBD2_COMMIT_BLOCK, SEQUENCE));
        journal.insert(4, revoke_with_block(100, SEQUENCE.wrapping_add(1)));
        journal.insert(5, block_header(JBD2_COMMIT_BLOCK, SEQUENCE.wrapping_add(1)));
        journal.insert(6, vec![0; BLOCK_SIZE]);

        let scan = scan_journal(&superblock, &journal).unwrap();
        let plan = plan_journal_replay(&scan);

        assert!(plan.is_empty());
        assert_eq!(plan.revoke_hit_count(), 1);
    }

    #[test]
    fn later_update_after_revoke_still_replays() {
        let superblock = journal_superblock(1);
        let mut journal = TestJournal::new();
        journal.insert(1, descriptor_with_tag(100, 0, SEQUENCE));
        journal.insert(2, vec![0x11; BLOCK_SIZE]);
        journal.insert(3, block_header(JBD2_COMMIT_BLOCK, SEQUENCE));
        journal.insert(4, revoke_with_block(100, SEQUENCE.wrapping_add(1)));
        journal.insert(5, block_header(JBD2_COMMIT_BLOCK, SEQUENCE.wrapping_add(1)));
        journal.insert(6, descriptor_with_tag(100, 0, SEQUENCE.wrapping_add(2)));
        journal.insert(7, vec![0x22; BLOCK_SIZE]);
        journal.insert(8, block_header(JBD2_COMMIT_BLOCK, SEQUENCE.wrapping_add(2)));
        journal.insert(9, vec![0; BLOCK_SIZE]);

        let scan = scan_journal(&superblock, &journal).unwrap();
        let plan = plan_journal_replay(&scan);

        assert_eq!(plan.revoke_hit_count(), 1);
        assert_eq!(plan.updates().len(), 1);
        assert_eq!(
            plan.updates()[0].transaction(),
            TransactionId::new(SEQUENCE.wrapping_add(2))
        );
        assert_eq!(plan.updates()[0].target(), JournalTargetBlock::new(100));
        assert_eq!(plan.updates()[0].log_block(), JournalBlock::new(7));
    }

    #[test]
    fn reads_escaped_replay_update_with_restored_magic() {
        let superblock = journal_superblock(1);
        let mut journal = TestJournal::new();
        let mut escaped = vec![0x33; BLOCK_SIZE];
        escaped[..4].copy_from_slice(&0u32.to_be_bytes());
        journal.insert(1, descriptor_with_tag(100, JBD2_FLAG_ESCAPE, SEQUENCE));
        journal.insert(2, escaped);
        journal.insert(3, block_header(JBD2_COMMIT_BLOCK, SEQUENCE));
        journal.insert(4, vec![0; BLOCK_SIZE]);

        let scan = scan_journal(&superblock, &journal).unwrap();
        let plan = plan_journal_replay(&scan);
        let mut output = vec![0; BLOCK_SIZE];
        read_journal_replay_update(&journal, &plan.updates()[0], &mut output).unwrap();

        assert_eq!(&output[..4], &JBD2_MAGIC_NUMBER.to_be_bytes());
        assert_eq!(output[4], 0x33);
    }

    #[test]
    fn executes_replay_plan_and_flushes_writer() {
        let superblock = journal_superblock(1);
        let mut journal = TestJournal::new();
        let mut escaped = vec![0x44; BLOCK_SIZE];
        escaped[..4].copy_from_slice(&0u32.to_be_bytes());
        journal.insert(1, descriptor_with_tag(100, JBD2_FLAG_ESCAPE, SEQUENCE));
        journal.insert(2, escaped);
        journal.insert(3, block_header(JBD2_COMMIT_BLOCK, SEQUENCE));
        journal.insert(4, vec![0; BLOCK_SIZE]);

        let scan = scan_journal(&superblock, &journal).unwrap();
        let plan = plan_journal_replay(&scan);
        let writer = TestWriter::new();
        let report = replay_journal_plan(&journal, &writer, &plan, BLOCK_SIZE).unwrap();

        let blocks = writer.blocks.borrow();
        let written = blocks.get(&100).expect("target block was replayed");
        assert_eq!(&written[..4], &JBD2_MAGIC_NUMBER.to_be_bytes());
        assert_eq!(written[4], 0x44);
        assert_eq!(
            report,
            JournalReplayReport {
                update_count: 1,
                revoke_hit_count: 0,
                head: JournalBlock::new(4),
                next_sequence: TransactionId::new(SEQUENCE.wrapping_add(1)),
            }
        );
        assert!(*writer.is_flushed.borrow());
    }

    #[test]
    fn executes_scanned_replay_and_flushes_writer_without_plan_vector() {
        let superblock = journal_superblock(1);
        let mut journal = TestJournal::new();
        let mut escaped = vec![0x55; BLOCK_SIZE];
        escaped[..4].copy_from_slice(&0u32.to_be_bytes());
        journal.insert(1, descriptor_with_tag(100, JBD2_FLAG_ESCAPE, SEQUENCE));
        journal.insert(2, escaped);
        journal.insert(3, block_header(JBD2_COMMIT_BLOCK, SEQUENCE));
        journal.insert(4, revoke_with_block(200, SEQUENCE.wrapping_add(1)));
        journal.insert(5, block_header(JBD2_COMMIT_BLOCK, SEQUENCE.wrapping_add(1)));
        journal.insert(6, vec![0; BLOCK_SIZE]);

        let scan = scan_journal(&superblock, &journal).unwrap();
        let writer = TestWriter::new();
        let applied = replay_scanned_journal(&journal, &writer, &scan, BLOCK_SIZE).unwrap();

        let blocks = writer.blocks.borrow();
        let written = blocks.get(&100).expect("target block was replayed");
        assert_eq!(&written[..4], &JBD2_MAGIC_NUMBER.to_be_bytes());
        assert_eq!(written[4], 0x55);
        assert_eq!(
            applied.into_report(),
            JournalReplayReport {
                update_count: 1,
                revoke_hit_count: 0,
                head: JournalBlock::new(6),
                next_sequence: TransactionId::new(SEQUENCE.wrapping_add(2)),
            }
        );
        assert!(*writer.is_flushed.borrow());
    }

    fn journal_superblock(start: u32) -> JournalSuperblock {
        let mut bytes = [0; BLOCK_SIZE];
        put_be_u32(&mut bytes, 0, JBD2_MAGIC_NUMBER);
        put_be_u32(&mut bytes, 4, 4);
        put_be_u32(&mut bytes, 12, BLOCK_SIZE as u32);
        put_be_u32(&mut bytes, 16, JOURNAL_BLOCKS);
        put_be_u32(&mut bytes, 20, 1);
        put_be_u32(&mut bytes, 24, SEQUENCE);
        put_be_u32(&mut bytes, 28, start);
        put_be_u32(&mut bytes, 40, JBD2_FEATURE_INCOMPAT_REVOKE);
        bytes[48..64].copy_from_slice(&UUID);
        put_be_u32(&mut bytes, 64, 1);
        JournalSuperblock::decode(&bytes, BLOCK_SIZE as u32, JOURNAL_BLOCKS, UUID).unwrap()
    }

    fn descriptor_with_tag(target: u32, flags: u16, sequence: u32) -> Vec<u8> {
        let mut bytes = block_header(JBD2_DESCRIPTOR_BLOCK, sequence);
        put_be_u32(&mut bytes, 12, target);
        put_be_u16(&mut bytes, 18, flags | JBD2_FLAG_LAST_TAG);
        if flags & JBD2_FLAG_SAME_UUID == 0 {
            bytes[20..36].copy_from_slice(&UUID);
        }
        bytes
    }

    fn revoke_with_block(target: u32, sequence: u32) -> Vec<u8> {
        let mut bytes = block_header(JBD2_REVOKE_BLOCK, sequence);
        put_be_u32(&mut bytes, 12, 20);
        put_be_u32(&mut bytes, 16, target);
        bytes
    }

    fn block_header(block_type: u32, sequence: u32) -> Vec<u8> {
        let mut bytes = vec![0; BLOCK_SIZE];
        put_be_u32(&mut bytes, 0, JBD2_MAGIC_NUMBER);
        put_be_u32(&mut bytes, 4, block_type);
        put_be_u32(&mut bytes, 8, sequence);
        bytes
    }

    fn put_be_u16(output: &mut [u8], offset: usize, value: u16) {
        output[offset..offset + 2].copy_from_slice(&value.to_be_bytes());
    }

    fn put_be_u32(output: &mut [u8], offset: usize, value: u32) {
        output[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
    }
}

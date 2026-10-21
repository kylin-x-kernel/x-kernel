// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! JBD2 recovery replay planning.

#[cfg(unittest)]
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
    #[cfg(unittest)]
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
#[cfg(unittest)]
#[derive(Clone, Debug, Eq, PartialEq)]
struct JournalReplayPlan {
    updates: Vec<JournalReplayUpdate>,
    head: JournalBlock,
    next_sequence: TransactionId,
    revoke_hit_count: usize,
}

#[cfg(unittest)]
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
pub(crate) struct JournalReplayReport {
    update_count: usize,
    revoke_hit_count: usize,
    head: JournalBlock,
    next_sequence: TransactionId,
}

impl JournalReplayReport {
    /// Returns how many metadata blocks were written by replay.
    pub(crate) const fn update_count(&self) -> usize {
        self.update_count
    }

    /// Returns how many descriptor updates were suppressed by revoke records.
    pub(crate) const fn revoke_hit_count(&self) -> usize {
        self.revoke_hit_count
    }

    /// Returns the first journal block after the recovered log contents.
    pub(crate) const fn head(&self) -> JournalBlock {
        self.head
    }

    /// Returns the sequence expected for the next transaction.
    pub(crate) const fn next_sequence(&self) -> TransactionId {
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
#[cfg(unittest)]
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
#[cfg(unittest)]
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

unittest::path_mod_test!(tests, "../../ut-tests/jbd2/replay/tests.rs");

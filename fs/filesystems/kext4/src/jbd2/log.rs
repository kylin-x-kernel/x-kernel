// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Checked parsing of the JBD2 circular transaction log.

use alloc::{vec, vec::Vec};

use crate::{
    disk::{checksum, codec},
    error::{ChecksumTarget, CorruptKind, Ext4Error, Ext4Result, UnsupportedKind},
    jbd2::{
        mapper::{JournalBlock, JournalTargetBlock, TransactionId},
        superblock::{
            FEATURE_COMPAT_CHECKSUM, FEATURE_INCOMPAT_64BIT, FEATURE_INCOMPAT_CSUM_V2,
            FEATURE_INCOMPAT_CSUM_V3, FEATURE_INCOMPAT_REVOKE, JournalStart, JournalSuperblock,
        },
    },
};

const JBD2_MAGIC_NUMBER: u32 = 0xc03b_3998;
const JBD2_DESCRIPTOR_BLOCK: u32 = 1;
const JBD2_COMMIT_BLOCK: u32 = 2;
const JBD2_REVOKE_BLOCK: u32 = 5;
const JBD2_HEADER_SIZE: usize = 12;
const JBD2_REVOKE_HEADER_SIZE: usize = 16;
const JBD2_BLOCK_TAIL_SIZE: usize = 4;
const MAX_SCANNED_TRANSACTIONS: usize = 65_536;
const MAX_SCANNED_UPDATES: usize = 262_144;
const MAX_SCANNED_REVOKES: usize = 262_144;

const JBD2_FLAG_ESCAPE: u32 = 1;
const JBD2_FLAG_SAME_UUID: u32 = 2;
const JBD2_FLAG_DELETED: u32 = 4;
const JBD2_FLAG_LAST_TAG: u32 = 8;
const JBD2_KNOWN_TAG_FLAGS: u32 =
    JBD2_FLAG_ESCAPE | JBD2_FLAG_SAME_UUID | JBD2_FLAG_DELETED | JBD2_FLAG_LAST_TAG;

/// Reads logical blocks from one JBD2 journal.
pub(crate) trait JournalBlockReader {
    /// Reads one complete journal block into `output`.
    fn read_journal_block(&self, block: JournalBlock, output: &mut [u8]) -> Ext4Result<()>;
}

/// One metadata update described by a JBD2 descriptor tag.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct JournalUpdate {
    target: JournalTargetBlock,
    log_block: JournalBlock,
    is_escaped: bool,
}

impl JournalUpdate {
    /// Returns the filesystem block that replay would update.
    pub(crate) const fn target(&self) -> JournalTargetBlock {
        self.target
    }

    /// Returns the journal block containing the replacement contents.
    pub(crate) const fn log_block(&self) -> JournalBlock {
        self.log_block
    }

    /// Returns whether replay must restore the escaped JBD2 magic value.
    pub(crate) const fn is_escaped(&self) -> bool {
        self.is_escaped
    }
}

/// Whether a scanned transaction has a valid commit record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum JournalTransactionState {
    /// The transaction ended with a valid commit block.
    Committed,
    /// The transaction ended before a matching commit block was found.
    Uncommitted,
}

/// A transaction reconstructed from descriptor, data, revoke, and commit blocks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct JournalTransaction {
    sequence: TransactionId,
    start: JournalBlock,
    next_log_block: JournalBlock,
    updates: Vec<JournalUpdate>,
    revoked_blocks: Vec<JournalTargetBlock>,
    state: JournalTransactionState,
}

impl JournalTransaction {
    /// Returns the JBD2 transaction sequence.
    pub(crate) const fn sequence(&self) -> TransactionId {
        self.sequence
    }

    /// Returns the first log block consumed by the transaction.
    #[cfg(unittest)]
    pub(crate) const fn start(&self) -> JournalBlock {
        self.start
    }

    /// Returns the first log block after the scanned transaction.
    #[cfg(unittest)]
    pub(crate) const fn next_log_block(&self) -> JournalBlock {
        self.next_log_block
    }

    /// Returns metadata updates in journal order.
    pub(crate) fn updates(&self) -> &[JournalUpdate] {
        &self.updates
    }

    /// Returns filesystem blocks revoked by this transaction.
    pub(crate) fn revoked_blocks(&self) -> &[JournalTargetBlock] {
        &self.revoked_blocks
    }

    /// Returns whether the transaction has a valid commit record.
    pub(crate) const fn state(&self) -> JournalTransactionState {
        self.state
    }
}

/// Result of scanning the active portion of a JBD2 circular log.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct JournalLogScan {
    transactions: Vec<JournalTransaction>,
    head: JournalBlock,
    next_sequence: TransactionId,
}

impl JournalLogScan {
    /// Returns transactions in log order.
    pub(crate) fn transactions(&self) -> &[JournalTransaction] {
        &self.transactions
    }

    /// Returns the first block after the scanned log contents.
    pub(crate) const fn head(&self) -> JournalBlock {
        self.head
    }

    /// Returns the sequence expected for the next transaction.
    pub(crate) const fn next_sequence(&self) -> TransactionId {
        self.next_sequence
    }
}

/// Scans the active JBD2 log without modifying filesystem storage.
///
/// Checksum v2 and v3 journals are verified while scanning. Legacy checksum
/// v1 is rejected because its transaction-wide CRC has different semantics.
pub(crate) fn scan_journal(
    superblock: &JournalSuperblock,
    reader: &impl JournalBlockReader,
) -> Ext4Result<JournalLogScan> {
    if superblock.feature_compat() & FEATURE_COMPAT_CHECKSUM != 0 {
        return Err(Ext4Error::UnsupportedJournalFeature {
            compat: superblock.feature_compat(),
            incompat: superblock.feature_incompat(),
            read_only_compat: superblock.feature_read_only_compat(),
        });
    }

    let start = match superblock.start() {
        JournalStart::Zero => {
            return Ok(JournalLogScan {
                transactions: Vec::new(),
                head: superblock.first_log_block(),
                next_sequence: superblock.sequence(),
            });
        }
        JournalStart::Block(block) => block,
    };

    LogScanner::new(superblock, reader)?.scan(start)
}

struct LogScanner<'a, R> {
    superblock: &'a JournalSuperblock,
    reader: &'a R,
    checksum_seed: u32,
    block: Vec<u8>,
    data_block: Vec<u8>,
    remaining_blocks: u32,
}

impl<'a, R: JournalBlockReader> LogScanner<'a, R> {
    fn new(superblock: &'a JournalSuperblock, reader: &'a R) -> Ext4Result<Self> {
        let block_size =
            usize::try_from(superblock.block_size()).map_err(|_| Ext4Error::Overflow)?;
        let remaining_blocks = superblock
            .max_blocks()
            .checked_sub(superblock.first_log_block().get())
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidJournal))?;
        let checksum_seed = checksum::crc32c(u32::MAX, &superblock.uuid());
        Ok(Self {
            superblock,
            reader,
            checksum_seed,
            block: vec![0; block_size],
            data_block: vec![0; block_size],
            remaining_blocks,
        })
    }

    fn scan(mut self, start: JournalBlock) -> Ext4Result<JournalLogScan> {
        let mut transactions = Vec::new();
        let mut current = None;
        let mut cursor = start;
        let mut expected_sequence = self.superblock.sequence();
        let mut total_updates = 0usize;
        let mut total_revokes = 0usize;

        while self.remaining_blocks != 0 {
            let control_block = cursor;
            self.read_block(control_block)?;

            let Some(header) = parse_header(&self.block)? else {
                // A block without the JBD2 magic marks the clean tail of the
                // active log range.
                break;
            };
            if header.sequence != expected_sequence {
                // A different sequence belongs to older circular-log
                // contents and is not part of the recoverable transaction set.
                break;
            }
            if !matches!(
                header.block_type,
                JBD2_DESCRIPTOR_BLOCK | JBD2_REVOKE_BLOCK | JBD2_COMMIT_BLOCK
            ) {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal));
            }
            self.consume_block(&mut cursor)?;

            match header.block_type {
                JBD2_DESCRIPTOR_BLOCK => {
                    let builder = current.get_or_insert_with(|| {
                        TransactionBuilder::new(expected_sequence, control_block)
                    });
                    self.verify_control_block_checksum(control_block)?;
                    let tags = parse_descriptor_tags(self.superblock, &self.block)?;
                    for tag in tags {
                        if self.remaining_blocks == 0 {
                            return Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal));
                        }
                        let log_block = cursor;
                        self.reader
                            .read_journal_block(log_block, &mut self.data_block)?;
                        self.verify_data_block_checksum(
                            expected_sequence,
                            log_block,
                            tag.checksum,
                        )?;
                        self.consume_block(&mut cursor)?;
                        ensure_scan_limit(total_updates, 1, MAX_SCANNED_UPDATES)?;
                        total_updates = total_updates.checked_add(1).ok_or(Ext4Error::Overflow)?;
                        builder.updates.push(JournalUpdate {
                            target: tag.target,
                            log_block,
                            is_escaped: tag.flags & JBD2_FLAG_ESCAPE != 0,
                        });
                    }
                }
                JBD2_REVOKE_BLOCK => {
                    if !has_revoke(self.superblock) {
                        return Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal));
                    }
                    let builder = current.get_or_insert_with(|| {
                        TransactionBuilder::new(expected_sequence, control_block)
                    });
                    self.verify_control_block_checksum(control_block)?;
                    let revoked_blocks = parse_revoke_blocks(self.superblock, &self.block)?;
                    ensure_scan_limit(total_revokes, revoked_blocks.len(), MAX_SCANNED_REVOKES)?;
                    total_revokes = total_revokes
                        .checked_add(revoked_blocks.len())
                        .ok_or(Ext4Error::Overflow)?;
                    builder.revoked_blocks.extend(revoked_blocks);
                }
                JBD2_COMMIT_BLOCK => {
                    self.verify_commit_block_checksum(control_block)?;
                    let builder = current.take().unwrap_or_else(|| {
                        TransactionBuilder::new(expected_sequence, control_block)
                    });
                    ensure_scan_limit(transactions.len(), 1, MAX_SCANNED_TRANSACTIONS)?;
                    transactions.push(builder.finish(cursor, JournalTransactionState::Committed));
                    expected_sequence = TransactionId::new(expected_sequence.get().wrapping_add(1));
                }
                _ => return Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal)),
            }
        }

        if let Some(builder) = current {
            ensure_scan_limit(transactions.len(), 1, MAX_SCANNED_TRANSACTIONS)?;
            transactions.push(builder.finish(cursor, JournalTransactionState::Uncommitted));
        }
        Ok(JournalLogScan {
            transactions,
            head: cursor,
            next_sequence: expected_sequence,
        })
    }

    fn read_block(&mut self, block: JournalBlock) -> Ext4Result<()> {
        self.reader.read_journal_block(block, &mut self.block)
    }

    fn consume_block(&mut self, cursor: &mut JournalBlock) -> Ext4Result<()> {
        self.remaining_blocks = self
            .remaining_blocks
            .checked_sub(1)
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidJournal))?;
        *cursor = next_log_block(self.superblock, *cursor);
        Ok(())
    }

    fn verify_control_block_checksum(&self, block: JournalBlock) -> Ext4Result<()> {
        if !has_checksum_v2_or_v3(self.superblock) {
            return Ok(());
        }
        let checksum_offset = self
            .block
            .len()
            .checked_sub(JBD2_BLOCK_TAIL_SIZE)
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidJournal))?;
        let actual = be_u32(&self.block, checksum_offset)?;
        let expected = checksum_with_zeroed_u32(self.checksum_seed, &self.block, checksum_offset)?;
        verify_checksum(block, expected, actual)
    }

    fn verify_commit_block_checksum(&self, block: JournalBlock) -> Ext4Result<()> {
        if !has_checksum_v2_or_v3(self.superblock) {
            return Ok(());
        }
        const COMMIT_CHECKSUM_OFFSET: usize = 16;
        let actual = be_u32(&self.block, COMMIT_CHECKSUM_OFFSET)?;
        let expected =
            checksum_with_zeroed_u32(self.checksum_seed, &self.block, COMMIT_CHECKSUM_OFFSET)?;
        verify_checksum(block, expected, actual)
    }

    fn verify_data_block_checksum(
        &self,
        sequence: TransactionId,
        block: JournalBlock,
        actual: Option<u32>,
    ) -> Ext4Result<()> {
        let Some(actual) = actual else {
            return Ok(());
        };
        let checksum = checksum::crc32c(self.checksum_seed, &sequence.get().to_be_bytes());
        let expected = checksum::crc32c(checksum, &self.data_block);
        let expected = if has_checksum_v3(self.superblock) {
            expected
        } else {
            expected & u32::from(u16::MAX)
        };
        verify_checksum(block, expected, actual)
    }
}

fn ensure_scan_limit(current: usize, added: usize, limit: usize) -> Ext4Result<()> {
    let next = current.checked_add(added).ok_or(Ext4Error::Overflow)?;
    if next > limit {
        return Err(Ext4Error::Unsupported(UnsupportedKind::JournalTooLarge));
    }
    Ok(())
}

struct TransactionBuilder {
    sequence: TransactionId,
    start: JournalBlock,
    updates: Vec<JournalUpdate>,
    revoked_blocks: Vec<JournalTargetBlock>,
}

impl TransactionBuilder {
    fn new(sequence: TransactionId, start: JournalBlock) -> Self {
        Self {
            sequence,
            start,
            updates: Vec::new(),
            revoked_blocks: Vec::new(),
        }
    }

    fn finish(
        self,
        next_log_block: JournalBlock,
        state: JournalTransactionState,
    ) -> JournalTransaction {
        JournalTransaction {
            sequence: self.sequence,
            start: self.start,
            next_log_block,
            updates: self.updates,
            revoked_blocks: self.revoked_blocks,
            state,
        }
    }
}

#[derive(Clone, Copy)]
struct JournalHeader {
    block_type: u32,
    sequence: TransactionId,
}

fn parse_header(input: &[u8]) -> Ext4Result<Option<JournalHeader>> {
    if be_u32(input, 0)? != JBD2_MAGIC_NUMBER {
        return Ok(None);
    }
    Ok(Some(JournalHeader {
        block_type: be_u32(input, 4)?,
        sequence: TransactionId::new(be_u32(input, 8)?),
    }))
}

struct DescriptorTag {
    target: JournalTargetBlock,
    flags: u32,
    checksum: Option<u32>,
}

fn parse_descriptor_tags(
    superblock: &JournalSuperblock,
    input: &[u8],
) -> Ext4Result<Vec<DescriptorTag>> {
    let has_checksum = has_checksum_v2_or_v3(superblock);
    let end = if has_checksum {
        input
            .len()
            .checked_sub(JBD2_BLOCK_TAIL_SIZE)
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidJournal))?
    } else {
        input.len()
    };
    let tag_size = journal_tag_size(superblock);
    let mut offset = JBD2_HEADER_SIZE;
    let mut tags = Vec::new();

    loop {
        let tag_end = offset
            .checked_add(tag_size)
            .filter(|end_offset| *end_offset <= end)
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidJournal))?;
        let low = u64::from(be_u32(input, offset)?);
        let (flags, high, tag_checksum) = if has_checksum_v3(superblock) {
            let high = if has_64bit(superblock) {
                u64::from(be_u32(input, checked_offset(offset, 8)?)?)
            } else {
                0
            };
            (
                be_u32(input, checked_offset(offset, 4)?)?,
                high,
                Some(be_u32(input, checked_offset(offset, 12)?)?),
            )
        } else {
            let flags = u32::from(be_u16(input, checked_offset(offset, 6)?)?);
            let high = if has_64bit(superblock) {
                u64::from(be_u32(input, checked_offset(offset, 8)?)?)
            } else {
                0
            };
            let tag_checksum = if has_checksum {
                Some(u32::from(be_u16(input, checked_offset(offset, 4)?)?))
            } else {
                None
            };
            (flags, high, tag_checksum)
        };
        if flags & !JBD2_KNOWN_TAG_FLAGS != 0 {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal));
        }
        if flags & JBD2_FLAG_DELETED != 0 {
            return Err(Ext4Error::Unsupported(
                UnsupportedKind::DeletedJournalUpdate,
            ));
        }

        offset = tag_end;
        if has_checksum_v3(superblock) {
            tags.push(DescriptorTag {
                target: JournalTargetBlock::new((high << 32) | low),
                flags,
                checksum: tag_checksum,
            });
            if flags & JBD2_FLAG_LAST_TAG != 0 {
                return Ok(tags);
            }
            continue;
        }

        if flags & JBD2_FLAG_SAME_UUID != 0 && tags.is_empty() {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal));
        }

        if flags & JBD2_FLAG_LAST_TAG != 0 {
            tags.push(DescriptorTag {
                target: JournalTargetBlock::new((high << 32) | low),
                flags,
                checksum: tag_checksum,
            });
            return Ok(tags);
        }

        if flags & JBD2_FLAG_SAME_UUID == 0 {
            let uuid_end = offset
                .checked_add(16)
                .filter(|uuid_end| *uuid_end <= end)
                .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidJournal))?;
            if input.get(offset..uuid_end) != Some(superblock.uuid().as_slice()) {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal));
            }
            offset = uuid_end;
        }

        tags.push(DescriptorTag {
            target: JournalTargetBlock::new((high << 32) | low),
            flags,
            checksum: tag_checksum,
        });
    }
}

fn parse_revoke_blocks(
    superblock: &JournalSuperblock,
    input: &[u8],
) -> Ext4Result<Vec<JournalTargetBlock>> {
    let count = usize::try_from(be_u32(input, 12)?).map_err(|_| Ext4Error::Overflow)?;
    let end = if has_checksum_v2_or_v3(superblock) {
        input
            .len()
            .checked_sub(JBD2_BLOCK_TAIL_SIZE)
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidJournal))?
    } else {
        input.len()
    };
    if !(JBD2_REVOKE_HEADER_SIZE..=end).contains(&count) {
        return Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal));
    }

    let entry_size = if has_64bit(superblock) { 8 } else { 4 };
    if !(count - JBD2_REVOKE_HEADER_SIZE).is_multiple_of(entry_size) {
        return Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal));
    }
    let mut revoked_blocks = Vec::new();
    let mut offset = JBD2_REVOKE_HEADER_SIZE;
    while offset < count {
        let block = if has_64bit(superblock) {
            be_u64(input, offset)?
        } else {
            u64::from(be_u32(input, offset)?)
        };
        revoked_blocks.push(JournalTargetBlock::new(block));
        offset = checked_offset(offset, entry_size)?;
    }
    Ok(revoked_blocks)
}

fn journal_tag_size(superblock: &JournalSuperblock) -> usize {
    if has_checksum_v3(superblock) {
        return 16;
    }
    if has_64bit(superblock) { 12 } else { 8 }
}

fn has_64bit(superblock: &JournalSuperblock) -> bool {
    superblock.feature_incompat() & FEATURE_INCOMPAT_64BIT != 0
}

fn has_revoke(superblock: &JournalSuperblock) -> bool {
    superblock.feature_incompat() & FEATURE_INCOMPAT_REVOKE != 0
}

fn has_checksum_v3(superblock: &JournalSuperblock) -> bool {
    superblock.feature_incompat() & FEATURE_INCOMPAT_CSUM_V3 != 0
}

fn has_checksum_v2_or_v3(superblock: &JournalSuperblock) -> bool {
    superblock.feature_incompat() & (FEATURE_INCOMPAT_CSUM_V2 | FEATURE_INCOMPAT_CSUM_V3) != 0
}

fn next_log_block(superblock: &JournalSuperblock, block: JournalBlock) -> JournalBlock {
    debug_assert!(
        (superblock.first_log_block().get()..superblock.max_blocks()).contains(&block.get()),
        "journal block {block} is outside the circular log range"
    );
    let next = block.get() + 1;
    if next == superblock.max_blocks() {
        superblock.first_log_block()
    } else {
        JournalBlock::new(next)
    }
}

fn checksum_with_zeroed_u32(seed: u32, input: &[u8], offset: usize) -> Ext4Result<u32> {
    let after_offset = checked_offset(offset, 4)?;
    let before = input
        .get(..offset)
        .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidJournal))?;
    let after = input
        .get(after_offset..)
        .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidJournal))?;
    let checksum = checksum::crc32c(seed, before);
    let checksum = checksum::crc32c(checksum, &[0; 4]);
    Ok(checksum::crc32c(checksum, after))
}

fn verify_checksum(block: JournalBlock, expected: u32, actual: u32) -> Ext4Result<()> {
    if expected == actual {
        Ok(())
    } else {
        Err(Ext4Error::ChecksumMismatch {
            target: ChecksumTarget::JournalBlock(block.get()),
            expected,
            actual,
        })
    }
}

fn be_u16(input: &[u8], offset: usize) -> Ext4Result<u16> {
    Ok(u16::from_be_bytes(codec::bytes(input, offset)?))
}

fn be_u32(input: &[u8], offset: usize) -> Ext4Result<u32> {
    Ok(u32::from_be_bytes(codec::bytes(input, offset)?))
}

fn be_u64(input: &[u8], offset: usize) -> Ext4Result<u64> {
    Ok(u64::from_be_bytes(codec::bytes(input, offset)?))
}

fn checked_offset(offset: usize, len: usize) -> Ext4Result<usize> {
    offset.checked_add(len).ok_or(Ext4Error::Overflow)
}

unittest::path_mod_test!(tests, "../../ut-tests/jbd2/log/tests.rs");

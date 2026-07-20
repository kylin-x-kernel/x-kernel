// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Online JBD2 descriptor/data/commit block encoding.

use alloc::{sync::Arc, vec, vec::Vec};

use crate::{
    Ext4Error, Ext4Result, FilesystemBlock,
    disk::checksum,
    error::{CorruptKind, UnsupportedKind},
    jbd2::{
        JournalCommit,
        log::JournalBlockReader,
        mapper::{JournalBlock, JournalTargetBlock, TransactionId},
        superblock::{
            FEATURE_COMPAT_CHECKSUM, FEATURE_INCOMPAT_64BIT, FEATURE_INCOMPAT_CSUM_V2,
            FEATURE_INCOMPAT_CSUM_V3, FEATURE_INCOMPAT_REVOKE, JournalStart, JournalSuperblock,
            mark_superblock_active, mark_superblock_empty,
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

const JBD2_FLAG_ESCAPE: u32 = 1;
const JBD2_FLAG_SAME_UUID: u32 = 2;
const JBD2_FLAG_LAST_TAG: u32 = 8;

/// Writes logical blocks into one JBD2 journal.
pub(crate) trait JournalBlockWriter {
    /// Writes one complete journal block.
    fn write_journal_block(&self, block: JournalBlock, input: &[u8]) -> Ext4Result<()>;

    /// Flushes journal writes through the device barrier used by this stage.
    fn flush_journal(&self) -> Ext4Result<()>;
}

pub(crate) trait JournalIo: JournalBlockReader + JournalBlockWriter {}

impl<T: JournalBlockReader + JournalBlockWriter> JournalIo for T {}

/// One committed metadata image that must be copied into the journal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct JournalCommitBlock {
    target: FilesystemBlock,
    bytes: Arc<[u8]>,
}

impl JournalCommitBlock {
    pub(crate) fn new(target: FilesystemBlock, bytes: Arc<[u8]>) -> Self {
        Self { target, bytes }
    }

    pub(crate) const fn target(&self) -> FilesystemBlock {
        self.target
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Summary of an online journal commit write.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct JournalLogCommit {
    transaction: TransactionId,
    start: JournalBlock,
    head: JournalBlock,
    next_sequence: TransactionId,
    update_count: usize,
    revoke_count: usize,
    journal_block_count: u32,
}

impl JournalLogCommit {
    /// Returns the committed transaction sequence.
    pub(crate) const fn transaction(&self) -> TransactionId {
        self.transaction
    }

    /// Returns the first journal block written for this transaction.
    pub(crate) const fn start(&self) -> JournalBlock {
        self.start
    }

    /// Returns the first journal block after the written commit block.
    pub(crate) const fn head(&self) -> JournalBlock {
        self.head
    }

    /// Returns the next transaction sequence after this commit.
    pub(crate) const fn next_sequence(&self) -> TransactionId {
        self.next_sequence
    }

    /// Returns how many metadata updates were logged.
    #[cfg(test)]
    pub(crate) const fn update_count(&self) -> usize {
        self.update_count
    }

    /// Returns how many filesystem blocks were revoked.
    #[cfg(test)]
    pub(crate) const fn revoke_count(&self) -> usize {
        self.revoke_count
    }

    /// Returns how many journal blocks were consumed.
    #[cfg(test)]
    pub(crate) const fn journal_block_count(&self) -> u32 {
        self.journal_block_count
    }
}

/// Evidence that an online commit is discoverable by recovery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct JournalPersistedCommit {
    log: JournalLogCommit,
    superblock: JournalSuperblock,
}

impl JournalPersistedCommit {
    /// Returns the logged transaction summary.
    #[cfg(test)]
    pub(crate) const fn log(&self) -> JournalLogCommit {
        self.log
    }

    /// Returns the updated active journal superblock.
    pub(crate) fn superblock(&self) -> &JournalSuperblock {
        &self.superblock
    }
}

/// Writes a committed transaction and persists the journal as active.
///
/// The returned state means that a later crash can discover the committed
/// transaction through `scan_journal`. The caller must not checkpoint home
/// metadata blocks until this function succeeds.
pub(crate) fn persist_journal_commit(
    superblock: &JournalSuperblock,
    journal: &impl JournalIo,
    commit: &JournalCommit,
    blocks: &[JournalCommitBlock],
) -> Ext4Result<JournalPersistedCommit> {
    if superblock.has_nonzero_log_start() {
        return Err(Ext4Error::JournalBusy);
    }
    let start = clean_log_head(superblock);
    let log = write_journal_commit(superblock, journal, start, commit, blocks)?;
    let active_superblock = write_journal_superblock_active(superblock, journal, &log)?;
    Ok(JournalPersistedCommit {
        log,
        superblock: active_superblock,
    })
}

/// Marks a persisted transaction checkpointed in the journal superblock.
///
/// The caller must complete and flush home-block checkpoint I/O before calling
/// this function. Once it succeeds, recovery will treat the journal as clean.
pub(crate) fn finish_journal_checkpoint(
    superblock: &JournalSuperblock,
    journal: &impl JournalIo,
    persisted: &JournalPersistedCommit,
) -> Ext4Result<JournalSuperblock> {
    if superblock.start() != JournalStart::Block(persisted.log.start()) {
        return Err(Ext4Error::InvalidJournalTransaction);
    }
    write_journal_superblock_empty(superblock, journal, &persisted.log)
}

/// Writes descriptor, data, and commit blocks for one committed transaction.
///
/// The caller is responsible for selecting a free log range. This function
/// validates that the supplied metadata images exactly match the committed
/// transaction record, writes a JBD2 transaction that current recovery code can
/// scan, and flushes the journal device after the commit block is written.
pub(crate) fn write_journal_commit(
    superblock: &JournalSuperblock,
    writer: &impl JournalBlockWriter,
    start: JournalBlock,
    commit: &JournalCommit,
    blocks: &[JournalCommitBlock],
) -> Ext4Result<JournalLogCommit> {
    validate_supported_features(superblock)?;
    if !commit.revoked_blocks().is_empty() && !has_revoke(superblock) {
        return Err(Ext4Error::InvalidJournalTransaction);
    }
    validate_start(superblock, start)?;
    validate_commit_blocks(superblock, commit, blocks)?;

    let block_size = usize::try_from(superblock.block_size()).map_err(|_| Ext4Error::Overflow)?;
    let descriptor_capacity = descriptor_tag_capacity(superblock, block_size)?;
    let revoke_capacity = revoke_block_capacity(superblock, block_size)?;
    let descriptor_count = blocks.len().div_ceil(descriptor_capacity);
    let revoke_block_count = commit.revoked_blocks().len().div_ceil(revoke_capacity);
    let total_journal_blocks = descriptor_count
        .checked_add(blocks.len())
        .and_then(|count| count.checked_add(revoke_block_count))
        .and_then(|count| count.checked_add(1))
        .ok_or(Ext4Error::Overflow)?;
    let log_capacity = log_capacity(superblock)?;
    if total_journal_blocks > log_capacity {
        return Err(Ext4Error::Unsupported(UnsupportedKind::JournalTooLarge));
    }
    let journal_block_count =
        u32::try_from(total_journal_blocks).map_err(|_| Ext4Error::Overflow)?;

    let checksum_seed = checksum::crc32c(u32::MAX, &superblock.uuid());
    let mut cursor = start;
    let mut offset = 0usize;
    while offset < blocks.len() {
        let remaining = blocks.len() - offset;
        let group_len = remaining.min(descriptor_capacity);
        cursor = write_descriptor_group(
            superblock,
            writer,
            cursor,
            commit.id(),
            &blocks[offset..offset + group_len],
            checksum_seed,
            block_size,
        )?;
        offset += group_len;
    }
    let mut revoke_offset = 0usize;
    while revoke_offset < commit.revoked_blocks().len() {
        let remaining = commit.revoked_blocks().len() - revoke_offset;
        let group_len = remaining.min(revoke_capacity);
        cursor = write_revoke_block(
            superblock,
            writer,
            cursor,
            commit.id(),
            &commit.revoked_blocks()[revoke_offset..revoke_offset + group_len],
            checksum_seed,
            block_size,
        )?;
        revoke_offset += group_len;
    }

    let commit_block = encode_commit_block(superblock, commit.id(), checksum_seed, block_size)?;
    writer.write_journal_block(cursor, &commit_block)?;
    let head = next_log_block(superblock, cursor);
    writer.flush_journal()?;

    Ok(JournalLogCommit {
        transaction: commit.id(),
        start,
        head,
        next_sequence: TransactionId::new(commit.id().get().wrapping_add(1)),
        update_count: blocks.len(),
        revoke_count: commit.revoked_blocks().len(),
        journal_block_count,
    })
}

fn validate_supported_features(superblock: &JournalSuperblock) -> Ext4Result<()> {
    if superblock.feature_compat() & FEATURE_COMPAT_CHECKSUM != 0 {
        return Err(Ext4Error::UnsupportedJournalFeature {
            compat: superblock.feature_compat(),
            incompat: superblock.feature_incompat(),
            read_only_compat: superblock.feature_read_only_compat(),
        });
    }
    Ok(())
}

fn clean_log_head(superblock: &JournalSuperblock) -> JournalBlock {
    let head = superblock.head();
    if (superblock.first_log_block().get()..superblock.max_blocks()).contains(&head.get()) {
        head
    } else {
        superblock.first_log_block()
    }
}

fn write_journal_superblock_active(
    superblock: &JournalSuperblock,
    journal: &impl JournalIo,
    log: &JournalLogCommit,
) -> Ext4Result<JournalSuperblock> {
    update_journal_superblock(superblock, journal, |bytes| {
        mark_superblock_active(bytes, log.transaction(), log.start(), log.head())
    })
}

fn write_journal_superblock_empty(
    superblock: &JournalSuperblock,
    journal: &impl JournalIo,
    log: &JournalLogCommit,
) -> Ext4Result<JournalSuperblock> {
    update_journal_superblock(superblock, journal, |bytes| {
        mark_superblock_empty(bytes, log.next_sequence(), log.head())
    })
}

fn update_journal_superblock(
    superblock: &JournalSuperblock,
    journal: &impl JournalIo,
    update: impl FnOnce(&mut [u8]) -> Ext4Result<()>,
) -> Ext4Result<JournalSuperblock> {
    let block_size = usize::try_from(superblock.block_size()).map_err(|_| Ext4Error::Overflow)?;
    let mut bytes = vec![0; block_size];
    journal.read_journal_block(JournalBlock::new(0), &mut bytes)?;
    update(&mut bytes)?;
    journal.write_journal_block(JournalBlock::new(0), &bytes)?;
    journal.flush_journal()?;
    JournalSuperblock::decode(
        &bytes,
        superblock.block_size(),
        superblock.max_blocks(),
        superblock.uuid(),
    )
}

fn validate_start(superblock: &JournalSuperblock, start: JournalBlock) -> Ext4Result<()> {
    if (superblock.first_log_block().get()..superblock.max_blocks()).contains(&start.get()) {
        Ok(())
    } else {
        Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal))
    }
}

fn validate_commit_blocks(
    superblock: &JournalSuperblock,
    commit: &JournalCommit,
    blocks: &[JournalCommitBlock],
) -> Ext4Result<()> {
    let block_size = usize::try_from(superblock.block_size()).map_err(|_| Ext4Error::Overflow)?;
    if blocks.len() != commit.metadata_blocks().len() {
        return Err(Ext4Error::InvalidJournalTransaction);
    }
    if commit
        .revoked_blocks()
        .iter()
        .any(|revoked| commit.metadata_blocks().contains(revoked))
    {
        return Err(Ext4Error::InvalidJournalTransaction);
    }
    for (expected, block) in commit.metadata_blocks().iter().zip(blocks) {
        if *expected != block.target() {
            return Err(Ext4Error::InvalidJournalTransaction);
        }
        if block.bytes().len() != block_size {
            return Err(Ext4Error::InvalidBufferLength {
                expected: block_size,
                actual: block.bytes().len(),
            });
        }
    }
    Ok(())
}

fn log_capacity(superblock: &JournalSuperblock) -> Ext4Result<usize> {
    usize::try_from(
        superblock
            .max_blocks()
            .checked_sub(superblock.first_log_block().get())
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidJournal))?,
    )
    .map_err(|_| Ext4Error::Overflow)
}

fn descriptor_tag_capacity(superblock: &JournalSuperblock, block_size: usize) -> Ext4Result<usize> {
    let end = descriptor_payload_end(superblock, block_size)?;
    let mut count = 0usize;
    loop {
        let next_count = count.checked_add(1).ok_or(Ext4Error::Overflow)?;
        let size = JBD2_HEADER_SIZE
            .checked_add(descriptor_tag_area_size(superblock, next_count)?)
            .ok_or(Ext4Error::Overflow)?;
        if size > end {
            break;
        }
        count = next_count;
    }
    if count == 0 {
        return Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal));
    }
    Ok(count)
}

fn revoke_block_capacity(superblock: &JournalSuperblock, block_size: usize) -> Ext4Result<usize> {
    let end = descriptor_payload_end(superblock, block_size)?;
    let entry_size = revoke_entry_size(superblock);
    let payload = end
        .checked_sub(JBD2_REVOKE_HEADER_SIZE)
        .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidJournal))?;
    let count = payload / entry_size;
    if count == 0 {
        return Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal));
    }
    Ok(count)
}

fn descriptor_tag_area_size(superblock: &JournalSuperblock, tag_count: usize) -> Ext4Result<usize> {
    let tags = tag_count
        .checked_mul(journal_tag_size(superblock))
        .ok_or(Ext4Error::Overflow)?;
    if has_checksum_v3(superblock) || tag_count <= 1 {
        Ok(tags)
    } else {
        tags.checked_add(16).ok_or(Ext4Error::Overflow)
    }
}

fn descriptor_payload_end(superblock: &JournalSuperblock, block_size: usize) -> Ext4Result<usize> {
    if has_checksum_v2_or_v3(superblock) {
        block_size
            .checked_sub(JBD2_BLOCK_TAIL_SIZE)
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidJournal))
    } else {
        Ok(block_size)
    }
}

fn write_descriptor_group(
    superblock: &JournalSuperblock,
    writer: &impl JournalBlockWriter,
    descriptor_block: JournalBlock,
    transaction: TransactionId,
    blocks: &[JournalCommitBlock],
    checksum_seed: u32,
    block_size: usize,
) -> Ext4Result<JournalBlock> {
    let mut descriptor = block_header(JBD2_DESCRIPTOR_BLOCK, transaction, block_size)?;
    let mut data_blocks = Vec::with_capacity(blocks.len());
    let mut tag_offset = JBD2_HEADER_SIZE;
    for (index, block) in blocks.iter().enumerate() {
        let mut data = Vec::from(block.bytes());
        let mut flags = if escape_journal_data(&mut data) {
            JBD2_FLAG_ESCAPE
        } else {
            0
        };
        if index + 1 == blocks.len() {
            flags |= JBD2_FLAG_LAST_TAG;
        }
        if !has_checksum_v3(superblock) && index != 0 {
            flags |= JBD2_FLAG_SAME_UUID;
        }

        let data_checksum = data_block_checksum(superblock, checksum_seed, transaction, &data);
        write_descriptor_tag(
            superblock,
            &mut descriptor,
            tag_offset,
            JournalTargetBlock::new(block.target().get()),
            flags,
            data_checksum,
        )?;
        tag_offset = tag_offset
            .checked_add(journal_tag_size(superblock))
            .ok_or(Ext4Error::Overflow)?;
        if !has_checksum_v3(superblock) && index == 0 && blocks.len() > 1 {
            let uuid_end = tag_offset.checked_add(16).ok_or(Ext4Error::Overflow)?;
            descriptor
                .get_mut(tag_offset..uuid_end)
                .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidJournal))?
                .copy_from_slice(&superblock.uuid());
            tag_offset = uuid_end;
        }
        data_blocks.push(data);
    }
    set_control_block_checksum(superblock, &mut descriptor, checksum_seed)?;
    writer.write_journal_block(descriptor_block, &descriptor)?;

    let mut cursor = next_log_block(superblock, descriptor_block);
    for data in data_blocks {
        writer.write_journal_block(cursor, &data)?;
        cursor = next_log_block(superblock, cursor);
    }
    Ok(cursor)
}

fn write_revoke_block(
    superblock: &JournalSuperblock,
    writer: &impl JournalBlockWriter,
    revoke_block: JournalBlock,
    transaction: TransactionId,
    revoked_blocks: &[FilesystemBlock],
    checksum_seed: u32,
    block_size: usize,
) -> Ext4Result<JournalBlock> {
    let mut block = block_header(JBD2_REVOKE_BLOCK, transaction, block_size)?;
    let entry_size = revoke_entry_size(superblock);
    let count = JBD2_REVOKE_HEADER_SIZE
        .checked_add(
            revoked_blocks
                .len()
                .checked_mul(entry_size)
                .ok_or(Ext4Error::Overflow)?,
        )
        .ok_or(Ext4Error::Overflow)?;
    put_be_u32(
        &mut block,
        12,
        u32::try_from(count).map_err(|_| Ext4Error::Overflow)?,
    )?;
    let mut offset = JBD2_REVOKE_HEADER_SIZE;
    for revoked in revoked_blocks {
        let target = revoked.get();
        if has_64bit(superblock) {
            put_be_u64(&mut block, offset, target)?;
        } else {
            put_be_u32(
                &mut block,
                offset,
                u32::try_from(target).map_err(|_| Ext4Error::Overflow)?,
            )?;
        }
        offset = offset.checked_add(entry_size).ok_or(Ext4Error::Overflow)?;
    }
    set_control_block_checksum(superblock, &mut block, checksum_seed)?;
    writer.write_journal_block(revoke_block, &block)?;
    Ok(next_log_block(superblock, revoke_block))
}

fn escape_journal_data(data: &mut [u8]) -> bool {
    if data
        .get(..4)
        .is_some_and(|magic| magic == JBD2_MAGIC_NUMBER.to_be_bytes())
    {
        data[..4].copy_from_slice(&0u32.to_be_bytes());
        true
    } else {
        false
    }
}

fn write_descriptor_tag(
    superblock: &JournalSuperblock,
    output: &mut [u8],
    offset: usize,
    target: JournalTargetBlock,
    flags: u32,
    data_checksum: Option<u32>,
) -> Ext4Result<()> {
    let low = target.get() as u32;
    let high = (target.get() >> 32) as u32;
    put_be_u32(output, offset, low)?;
    if has_checksum_v3(superblock) {
        put_be_u32(output, checked_offset(offset, 4)?, flags)?;
        if has_64bit(superblock) {
            put_be_u32(output, checked_offset(offset, 8)?, high)?;
        }
        put_be_u32(
            output,
            checked_offset(offset, 12)?,
            data_checksum.unwrap_or(0),
        )?;
        return Ok(());
    }

    if let Some(checksum) = data_checksum {
        put_be_u16(output, checked_offset(offset, 4)?, checksum as u16)?;
    }
    put_be_u16(
        output,
        checked_offset(offset, 6)?,
        u16::try_from(flags).map_err(|_| Ext4Error::Overflow)?,
    )?;
    if has_64bit(superblock) {
        put_be_u32(output, checked_offset(offset, 8)?, high)?;
    }
    Ok(())
}

fn data_block_checksum(
    superblock: &JournalSuperblock,
    checksum_seed: u32,
    transaction: TransactionId,
    data: &[u8],
) -> Option<u32> {
    if !has_checksum_v2_or_v3(superblock) {
        return None;
    }
    let checksum = checksum::crc32c(checksum_seed, &transaction.get().to_be_bytes());
    let checksum = checksum::crc32c(checksum, data);
    if has_checksum_v3(superblock) {
        Some(checksum)
    } else {
        Some(checksum & u32::from(u16::MAX))
    }
}

fn encode_commit_block(
    superblock: &JournalSuperblock,
    transaction: TransactionId,
    checksum_seed: u32,
    block_size: usize,
) -> Ext4Result<Vec<u8>> {
    let mut block = block_header(JBD2_COMMIT_BLOCK, transaction, block_size)?;
    if has_checksum_v2_or_v3(superblock) {
        let checksum = checksum_with_zeroed_u32(checksum_seed, &block, 16)?;
        put_be_u32(&mut block, 16, checksum)?;
    }
    Ok(block)
}

fn block_header(
    block_type: u32,
    transaction: TransactionId,
    block_size: usize,
) -> Ext4Result<Vec<u8>> {
    let mut block = vec![0; block_size];
    put_be_u32(&mut block, 0, JBD2_MAGIC_NUMBER)?;
    put_be_u32(&mut block, 4, block_type)?;
    put_be_u32(&mut block, 8, transaction.get())?;
    Ok(block)
}

fn set_control_block_checksum(
    superblock: &JournalSuperblock,
    block: &mut [u8],
    checksum_seed: u32,
) -> Ext4Result<()> {
    if !has_checksum_v2_or_v3(superblock) {
        return Ok(());
    }
    let offset = block
        .len()
        .checked_sub(JBD2_BLOCK_TAIL_SIZE)
        .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidJournal))?;
    let checksum = checksum_with_zeroed_u32(checksum_seed, block, offset)?;
    put_be_u32(block, offset, checksum)
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

fn journal_tag_size(superblock: &JournalSuperblock) -> usize {
    if has_checksum_v3(superblock) {
        return 16;
    }
    if has_64bit(superblock) { 12 } else { 8 }
}

fn revoke_entry_size(superblock: &JournalSuperblock) -> usize {
    if has_64bit(superblock) { 8 } else { 4 }
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
    let next = block.get() + 1;
    if next == superblock.max_blocks() {
        superblock.first_log_block()
    } else {
        JournalBlock::new(next)
    }
}

fn put_be_u16(output: &mut [u8], offset: usize, value: u16) -> Ext4Result<()> {
    let end = checked_offset(offset, 2)?;
    output
        .get_mut(offset..end)
        .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidJournal))?
        .copy_from_slice(&value.to_be_bytes());
    Ok(())
}

fn put_be_u32(output: &mut [u8], offset: usize, value: u32) -> Ext4Result<()> {
    let end = checked_offset(offset, 4)?;
    output
        .get_mut(offset..end)
        .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidJournal))?
        .copy_from_slice(&value.to_be_bytes());
    Ok(())
}

fn put_be_u64(output: &mut [u8], offset: usize, value: u64) -> Ext4Result<()> {
    let end = checked_offset(offset, 8)?;
    output
        .get_mut(offset..end)
        .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidJournal))?
        .copy_from_slice(&value.to_be_bytes());
    Ok(())
}

fn checked_offset(offset: usize, len: usize) -> Ext4Result<usize> {
    offset.checked_add(len).ok_or(Ext4Error::Overflow)
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::BTreeMap};

    use super::*;
    use crate::{
        InodeNumber,
        jbd2::{
            Journal, JournalBlockReader, JournalCredits, JournalReplayBlockWriter,
            JournalSuperblock, replay_scanned_journal, scan_journal,
            superblock::JOURNAL_SUPERBLOCK_SIZE,
        },
    };

    const UUID: [u8; 16] = [0x5a; 16];
    const BLOCK_SIZE: usize = 1024;
    const JOURNAL_BLOCKS: u32 = 1024;
    const SEQUENCE: u32 = 42;

    struct TestJournal {
        blocks: RefCell<BTreeMap<u32, Vec<u8>>>,
        flush_count: RefCell<usize>,
    }

    struct TestReplayWriter {
        blocks: RefCell<BTreeMap<u64, Vec<u8>>>,
    }

    impl TestJournal {
        fn new() -> Self {
            Self {
                blocks: RefCell::new(BTreeMap::new()),
                flush_count: RefCell::new(0),
            }
        }

        fn with_superblock(superblock: &JournalSuperblock) -> Self {
            let journal = Self::new();
            journal.insert_superblock(superblock);
            journal
        }

        fn insert_superblock(&self, superblock: &JournalSuperblock) {
            let block = encoded_journal_superblock(superblock);
            self.blocks.borrow_mut().insert(0, block);
        }
    }

    impl JournalBlockWriter for TestJournal {
        fn write_journal_block(&self, block: JournalBlock, input: &[u8]) -> Ext4Result<()> {
            if input.len() != BLOCK_SIZE {
                return Err(Ext4Error::InvalidBufferLength {
                    expected: BLOCK_SIZE,
                    actual: input.len(),
                });
            }
            self.blocks.borrow_mut().insert(block.get(), input.to_vec());
            Ok(())
        }

        fn flush_journal(&self) -> Ext4Result<()> {
            *self.flush_count.borrow_mut() += 1;
            Ok(())
        }
    }

    impl JournalBlockReader for TestJournal {
        fn read_journal_block(&self, block: JournalBlock, output: &mut [u8]) -> Ext4Result<()> {
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

    #[test]
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

    #[test]
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

    #[test]
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

    #[test]
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

    #[test]
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

    #[test]
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

    #[test]
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

    #[test]
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
        assert!(journal.blocks.borrow().contains_key(&1023));
        assert!(journal.blocks.borrow().contains_key(&1));
        assert!(journal.blocks.borrow().contains_key(&2));
        let scan = scan_journal(&superblock, &journal).unwrap();
        assert_eq!(
            scan.transactions()[0].updates()[0].log_block(),
            JournalBlock::new(1)
        );
    }

    #[test]
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

    #[test]
    fn persisted_commit_marks_journal_active_and_recoverable() {
        let superblock = journal_superblock(0, 0, SEQUENCE);
        let journal = TestJournal::with_superblock(&superblock);
        let (commit, blocks) =
            committed_blocks(&[(FilesystemBlock::new(800), vec![0x88; BLOCK_SIZE])]);

        let persisted = persist_journal_commit(&superblock, &journal, &commit, &blocks).unwrap();

        assert_eq!(persisted.log().start(), JournalBlock::new(1));
        assert_eq!(persisted.log().head(), JournalBlock::new(4));
        assert_eq!(
            persisted.superblock().start(),
            JournalStart::Block(JournalBlock::new(1))
        );
        assert_eq!(persisted.superblock().head(), JournalBlock::new(4));
        assert_eq!(*journal.flush_count.borrow(), 2);

        let scan = scan_journal(persisted.superblock(), &journal).unwrap();
        assert_eq!(scan.transactions().len(), 1);
        let replay = TestReplayWriter::new();
        replay_scanned_journal(&journal, &replay, &scan, BLOCK_SIZE).unwrap();
        assert_eq!(replay.blocks.borrow().get(&800).unwrap()[0], 0x88);
    }

    #[test]
    fn checkpoint_finish_marks_journal_empty_after_persisted_commit() {
        let superblock = journal_superblock(0, FEATURE_INCOMPAT_CSUM_V3, SEQUENCE);
        let journal = TestJournal::with_superblock(&superblock);
        let (commit, blocks) =
            committed_blocks(&[(FilesystemBlock::new(900), vec![0x99; BLOCK_SIZE])]);

        let persisted = persist_journal_commit(&superblock, &journal, &commit, &blocks).unwrap();
        let clean =
            finish_journal_checkpoint(persisted.superblock(), &journal, &persisted).unwrap();

        assert_eq!(clean.start(), JournalStart::Zero);
        assert_eq!(clean.sequence(), TransactionId::new(SEQUENCE + 1));
        assert_eq!(clean.head(), persisted.log().head());
        assert_eq!(*journal.flush_count.borrow(), 3);
        let scan = scan_journal(&clean, &journal).unwrap();
        assert!(scan.transactions().is_empty());
    }

    #[test]
    fn persist_commit_rejects_already_active_journal() {
        let superblock = journal_superblock(1, 0, SEQUENCE);
        let journal = TestJournal::with_superblock(&superblock);
        let (commit, blocks) =
            committed_blocks(&[(FilesystemBlock::new(1000), vec![0xaa; BLOCK_SIZE])]);

        assert_eq!(
            persist_journal_commit(&superblock, &journal, &commit, &blocks).map(|_| ()),
            Err(Ext4Error::JournalBusy)
        );
    }

    fn committed_blocks(
        input: &[(FilesystemBlock, Vec<u8>)],
    ) -> (JournalCommit, Vec<JournalCommitBlock>) {
        committed_blocks_with_sequence(SEQUENCE, input)
    }

    fn committed_blocks_with_sequence(
        sequence: u32,
        input: &[(FilesystemBlock, Vec<u8>)],
    ) -> (JournalCommit, Vec<JournalCommitBlock>) {
        let journal = Journal::new(TransactionId::new(sequence));
        let mut handle = journal
            .begin(JournalCredits::new(input.len() as u32))
            .unwrap();
        handle.mark_inode_sync(InodeNumber::new(12)).unwrap();
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
    ) -> JournalCommit {
        let journal = Journal::new(TransactionId::new(sequence));
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

    fn encoded_journal_superblock(superblock: &JournalSuperblock) -> Vec<u8> {
        journal_superblock_bytes(
            match superblock.start() {
                JournalStart::Zero => 0,
                JournalStart::Block(block) => block.get(),
            },
            superblock.feature_incompat(),
            superblock.sequence().get(),
        )
        .into_iter()
        .collect()
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
}

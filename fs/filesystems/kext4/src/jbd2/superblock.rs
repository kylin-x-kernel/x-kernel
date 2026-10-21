// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{sync::Arc, vec, vec::Vec};
use core::num::NonZeroU32;

use crate::{
    disk::{checksum, codec},
    error::{ChecksumTarget, CorruptKind, Ext4Error, Ext4Result},
    jbd2::mapper::{JournalBlock, TransactionId},
};

pub(crate) const JOURNAL_SUPERBLOCK_SIZE: usize = 1024;

const JBD2_MAGIC_NUMBER: u32 = 0xc03b_3998;
const JBD2_SUPERBLOCK_V1: u32 = 3;
const JBD2_SUPERBLOCK_V2: u32 = 4;
const JBD2_MIN_JOURNAL_BLOCKS: u32 = 1024;
const JBD2_CRC32C_CHECKSUM: u8 = 4;

pub(super) const FEATURE_COMPAT_CHECKSUM: u32 = 0x0000_0001;

pub(super) const FEATURE_INCOMPAT_REVOKE: u32 = 0x0000_0001;
pub(super) const FEATURE_INCOMPAT_64BIT: u32 = 0x0000_0002;
pub(super) const FEATURE_INCOMPAT_CSUM_V2: u32 = 0x0000_0008;
pub(super) const FEATURE_INCOMPAT_CSUM_V3: u32 = 0x0000_0010;
const FEATURE_INCOMPAT_FAST_COMMIT: u32 = 0x0000_0020;

const SUPPORTED_INCOMPAT: u32 = FEATURE_INCOMPAT_REVOKE
    | FEATURE_INCOMPAT_64BIT
    | FEATURE_INCOMPAT_CSUM_V2
    | FEATURE_INCOMPAT_CSUM_V3;
const SEQUENCE_OFFSET: usize = 0x18;
const START_OFFSET: usize = 0x1c;
const HEAD_OFFSET: usize = 0x58;
const CHECKSUM_OFFSET: usize = 0xfc;

struct RawJournalSuperblock<'a> {
    bytes: &'a [u8; JOURNAL_SUPERBLOCK_SIZE],
}

impl<'a> TryFrom<&'a [u8]> for RawJournalSuperblock<'a> {
    type Error = Ext4Error;

    fn try_from(input: &'a [u8]) -> Ext4Result<Self> {
        let bytes = input
            .get(..JOURNAL_SUPERBLOCK_SIZE)
            .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?
            .try_into()
            .map_err(|_| Ext4Error::Corrupt(CorruptKind::Truncated))?;
        Ok(Self { bytes })
    }
}

/// Encoded start of the oldest transaction in the circular journal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JournalStart {
    /// The superblock currently records no log start.
    Zero,
    /// The recorded first log block.
    Block(JournalBlock),
}

/// Validated geometry and state from a JBD2 journal superblock.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournalSuperblock {
    encoded: Arc<[u8]>,
    block_size: NonZeroU32,
    max_blocks: NonZeroU32,
    first_log_block: JournalBlock,
    sequence: TransactionId,
    start: JournalStart,
    head: JournalBlock,
    error: u32,
    feature_compat: u32,
    feature_incompat: u32,
    feature_read_only_compat: u32,
    uuid: [u8; 16],
}

impl JournalSuperblock {
    pub(crate) fn decode(
        input: &[u8],
        filesystem_block_size: u32,
        journal_inode_blocks: u32,
        filesystem_uuid: [u8; 16],
    ) -> Ext4Result<Self> {
        let encoded_input = input;
        let raw = RawJournalSuperblock::try_from(input)?;
        let input = raw.bytes;
        if be_u32(input, 0x00)? != JBD2_MAGIC_NUMBER {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal));
        }

        let block_type = be_u32(input, 0x04)?;
        if !matches!(block_type, JBD2_SUPERBLOCK_V1 | JBD2_SUPERBLOCK_V2) {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal));
        }
        let block_size = NonZeroU32::new(be_u32(input, 0x0c)?)
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidJournal))?;
        if block_size.get() != filesystem_block_size {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal));
        }
        let encoded_len = usize::try_from(block_size.get()).map_err(|_| Ext4Error::Overflow)?;
        let encoded = match encoded_input.get(..encoded_len) {
            Some(encoded) => Arc::from(encoded),
            None if encoded_input.len() == JOURNAL_SUPERBLOCK_SIZE => {
                let mut encoded = vec![0; encoded_len];
                encoded[..JOURNAL_SUPERBLOCK_SIZE].copy_from_slice(encoded_input);
                Arc::from(encoded)
            }
            None => return Err(Ext4Error::Corrupt(CorruptKind::Truncated)),
        };

        let max_blocks = NonZeroU32::new(be_u32(input, 0x10)?)
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidJournal))?;
        let first_log_block = JournalBlock::new(be_u32(input, 0x14)?);
        if max_blocks.get() < JBD2_MIN_JOURNAL_BLOCKS
            || max_blocks.get() > journal_inode_blocks
            || first_log_block.get() == 0
            || first_log_block.get() >= max_blocks.get()
        {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal));
        }

        let start = match be_u32(input, 0x1c)? {
            0 => JournalStart::Zero,
            value if (first_log_block.get()..max_blocks.get()).contains(&value) => {
                JournalStart::Block(JournalBlock::new(value))
            }
            _ => return Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal)),
        };

        let (feature_compat, feature_incompat, feature_read_only_compat, uuid) =
            if block_type == JBD2_SUPERBLOCK_V2 {
                (
                    be_u32(input, 0x24)?,
                    be_u32(input, 0x28)?,
                    be_u32(input, 0x2c)?,
                    codec::bytes(input, 0x30)?,
                )
            } else {
                (0, 0, 0, filesystem_uuid)
            };
        validate_features(feature_compat, feature_incompat, feature_read_only_compat)?;
        if block_type == JBD2_SUPERBLOCK_V2 && uuid != filesystem_uuid {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal));
        }

        if feature_incompat & (FEATURE_INCOMPAT_CSUM_V2 | FEATURE_INCOMPAT_CSUM_V3) != 0 {
            if input[0x50] != JBD2_CRC32C_CHECKSUM {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal));
            }
            let expected = journal_superblock_checksum(input)?;
            let actual = be_u32(input, CHECKSUM_OFFSET)?;
            if expected != actual {
                return Err(Ext4Error::ChecksumMismatch {
                    target: ChecksumTarget::JournalSuperblock,
                    expected,
                    actual,
                });
            }
        }

        Ok(Self {
            encoded,
            block_size,
            max_blocks,
            first_log_block,
            sequence: TransactionId::new(be_u32(input, SEQUENCE_OFFSET)?),
            start,
            head: JournalBlock::new(be_u32(input, HEAD_OFFSET)?),
            error: be_u32(input, 0x20)?,
            feature_compat,
            feature_incompat,
            feature_read_only_compat,
            uuid,
        })
    }

    /// Returns the validated in-memory journal superblock image.
    ///
    /// Runtime updates clone this image instead of reading journal block zero
    /// again. The bytes always cover one complete journal block.
    pub(crate) fn encoded(&self) -> &[u8] {
        &self.encoded
    }

    /// Applies an update to the cached image and validates the resulting state.
    pub(crate) fn updated(
        &self,
        update: impl FnOnce(&mut [u8]) -> Ext4Result<()>,
    ) -> Ext4Result<(Vec<u8>, Self)> {
        let mut bytes = self.encoded.to_vec();
        update(&mut bytes)?;
        let updated = Self::decode(&bytes, self.block_size(), self.max_blocks(), self.uuid())?;
        Ok((bytes, updated))
    }

    /// Returns the journal block size.
    pub const fn block_size(&self) -> u32 {
        self.block_size.get()
    }

    /// Returns the number of blocks addressable by the journal.
    pub const fn max_blocks(&self) -> u32 {
        self.max_blocks.get()
    }

    /// Returns the first block used by the circular log.
    pub const fn first_log_block(&self) -> JournalBlock {
        self.first_log_block
    }

    /// Returns the transaction sequence recorded in the superblock.
    pub const fn sequence(&self) -> TransactionId {
        self.sequence
    }

    /// Returns the raw JBD2 log start state.
    ///
    /// A nonzero value is diagnostic information. Ext4 recovery is selected
    /// by its `INCOMPAT_RECOVER` feature and later JBD2 replay, not by this
    /// field alone.
    pub const fn start(&self) -> JournalStart {
        self.start
    }

    /// Returns whether the superblock records a nonzero log start.
    pub const fn has_nonzero_log_start(&self) -> bool {
        matches!(self.start, JournalStart::Block(_))
    }

    /// Returns the recorded log head.
    pub const fn head(&self) -> JournalBlock {
        self.head
    }

    /// Returns the persistent JBD2 abort error value.
    pub const fn error(&self) -> u32 {
        self.error
    }

    /// Returns the compatible JBD2 feature bitmap.
    pub const fn feature_compat(&self) -> u32 {
        self.feature_compat
    }

    /// Returns the incompatible JBD2 feature bitmap.
    pub const fn feature_incompat(&self) -> u32 {
        self.feature_incompat
    }

    /// Returns the read-only-compatible JBD2 feature bitmap.
    pub const fn feature_read_only_compat(&self) -> u32 {
        self.feature_read_only_compat
    }

    /// Returns the journal UUID.
    pub const fn uuid(&self) -> [u8; 16] {
        self.uuid
    }
}

pub(crate) fn mark_superblock_empty(
    input: &mut [u8],
    sequence: TransactionId,
    head: JournalBlock,
) -> Ext4Result<()> {
    let feature_incompat = validate_mutable_superblock(input)?;

    put_be_u32(input, SEQUENCE_OFFSET, sequence.get())?;
    put_be_u32(input, START_OFFSET, 0)?;
    put_be_u32(input, HEAD_OFFSET, head.get())?;
    update_superblock_checksum(input, feature_incompat)
}

/// Enables revoke records on a version-2 journal superblock.
///
/// Version-1 journals have no feature bitmap and return `false` unchanged.
pub(crate) fn enable_superblock_revoke(input: &mut [u8]) -> Ext4Result<bool> {
    let raw = RawJournalSuperblock::try_from(&*input)?;
    if be_u32(raw.bytes, 0x00)? != JBD2_MAGIC_NUMBER {
        return Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal));
    }
    match be_u32(raw.bytes, 0x04)? {
        JBD2_SUPERBLOCK_V1 => return Ok(false),
        JBD2_SUPERBLOCK_V2 => {}
        _ => return Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal)),
    }
    if be_u32(raw.bytes, START_OFFSET)? != 0 {
        return Err(Ext4Error::JournalBusy);
    }

    let compat = be_u32(raw.bytes, 0x24)?;
    let incompat = be_u32(raw.bytes, 0x28)?;
    let read_only_compat = be_u32(raw.bytes, 0x2c)?;
    validate_features(compat, incompat, read_only_compat)?;
    let incompat = incompat | FEATURE_INCOMPAT_REVOKE;
    validate_features(compat, incompat, read_only_compat)?;
    put_be_u32(input, 0x28, incompat)?;
    update_superblock_checksum(input, incompat)?;
    Ok(true)
}

pub(crate) fn mark_superblock_active(
    input: &mut [u8],
    sequence: TransactionId,
    start: JournalBlock,
) -> Ext4Result<()> {
    let raw = RawJournalSuperblock::try_from(&*input)?;
    let first_log_block = be_u32(raw.bytes, 0x14)?;
    let max_blocks = be_u32(raw.bytes, 0x10)?;
    if !(first_log_block..max_blocks).contains(&start.get()) {
        return Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal));
    }
    let feature_incompat = validate_mutable_superblock(input)?;

    put_be_u32(input, SEQUENCE_OFFSET, sequence.get())?;
    put_be_u32(input, START_OFFSET, start.get())?;
    update_superblock_checksum(input, feature_incompat)
}

fn validate_mutable_superblock(input: &mut [u8]) -> Ext4Result<u32> {
    let raw = RawJournalSuperblock::try_from(&*input)?;
    let block_type = be_u32(raw.bytes, 0x04)?;
    let (feature_compat, feature_incompat, feature_read_only_compat) =
        if block_type == JBD2_SUPERBLOCK_V2 {
            (
                be_u32(raw.bytes, 0x24)?,
                be_u32(raw.bytes, 0x28)?,
                be_u32(raw.bytes, 0x2c)?,
            )
        } else {
            (0, 0, 0)
        };
    validate_features(feature_compat, feature_incompat, feature_read_only_compat)?;
    Ok(feature_incompat)
}

fn update_superblock_checksum(input: &mut [u8], feature_incompat: u32) -> Ext4Result<()> {
    if feature_incompat & (FEATURE_INCOMPAT_CSUM_V2 | FEATURE_INCOMPAT_CSUM_V3) != 0 {
        let checksum = journal_superblock_checksum(input)?;
        put_be_u32(input, CHECKSUM_OFFSET, checksum)?;
    }
    Ok(())
}

fn validate_features(compat: u32, incompat: u32, read_only_compat: u32) -> Ext4Result<()> {
    let unsupported_incompat = incompat & !(SUPPORTED_INCOMPAT | FEATURE_INCOMPAT_FAST_COMMIT);
    if unsupported_incompat != 0
        || read_only_compat != 0
        || incompat & FEATURE_INCOMPAT_FAST_COMMIT != 0
    {
        return Err(Ext4Error::UnsupportedJournalFeature {
            compat,
            incompat,
            read_only_compat,
        });
    }
    let has_checksum_v1 = compat & FEATURE_COMPAT_CHECKSUM != 0;
    let has_checksum_v2 = incompat & FEATURE_INCOMPAT_CSUM_V2 != 0;
    let has_checksum_v3 = incompat & FEATURE_INCOMPAT_CSUM_V3 != 0;
    if (has_checksum_v2 && has_checksum_v3)
        || (has_checksum_v1 && (has_checksum_v2 || has_checksum_v3))
    {
        return Err(Ext4Error::Corrupt(CorruptKind::InvalidJournal));
    }
    Ok(())
}

fn journal_superblock_checksum(input: &[u8]) -> Ext4Result<u32> {
    let raw = RawJournalSuperblock::try_from(input)?;
    let input = raw.bytes;
    let after_offset = CHECKSUM_OFFSET.checked_add(4).ok_or(Ext4Error::Overflow)?;
    let before = input
        .get(..CHECKSUM_OFFSET)
        .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?;
    let after = input
        .get(after_offset..)
        .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?;
    let checksum = checksum::crc32c(u32::MAX, before);
    let checksum = checksum::crc32c(checksum, &[0; 4]);
    Ok(checksum::crc32c(checksum, after))
}

fn be_u32(input: &[u8], offset: usize) -> Ext4Result<u32> {
    Ok(u32::from_be_bytes(codec::bytes(input, offset)?))
}

fn put_be_u32(output: &mut [u8], offset: usize, value: u32) -> Ext4Result<()> {
    let end = offset.checked_add(4).ok_or(Ext4Error::Overflow)?;
    output
        .get_mut(offset..end)
        .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?
        .copy_from_slice(&value.to_be_bytes());
    Ok(())
}

unittest::path_mod_test!(tests, "../../ut-tests/jbd2/superblock/tests.rs");

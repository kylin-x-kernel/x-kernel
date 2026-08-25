// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use crate::{CorruptKind, Ext4Error, Ext4Result, disk::codec};

pub(crate) const DIRENT_HEADER_SIZE: usize = 8;
pub(crate) const DIRENT_NAME_MAX: usize = 255;
pub(crate) const DIRENT_TAIL_SIZE: usize = 12;
pub(crate) const DIRENT_TAIL_FILE_TYPE: u8 = 0xde;
pub(crate) const DX_ROOT_INFO_OFFSET: usize = 24;
pub(crate) const DX_ROOT_COUNT_LIMIT_OFFSET: usize = 32;
pub(crate) const DX_NODE_COUNT_LIMIT_OFFSET: usize = 8;
pub(crate) const DX_COUNT_LIMIT_SIZE: usize = 4;
pub(crate) const DX_ENTRY_SIZE: usize = 8;
pub(crate) const DX_TAIL_SIZE: usize = 8;
pub(crate) const DX_BLOCK_MASK: u32 = 0x0fff_ffff;
pub(crate) const DX_HTREE_LEVEL_COMPAT: u8 = 2;
pub(crate) const DX_HTREE_LEVEL: u8 = 3;
pub(crate) const DX_HASH_LEGACY: u8 = 0;
pub(crate) const DX_HASH_HALF_MD4: u8 = 1;
pub(crate) const DX_HASH_TEA: u8 = 2;
pub(crate) const DX_HASH_SIPHASH: u8 = 6;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DirectoryFileType {
    Unknown,
    RegularFile,
    Directory,
    CharacterDevice,
    BlockDevice,
    Fifo,
    Socket,
    Symlink,
}

impl DirectoryFileType {
    pub(crate) const fn from_raw(value: u8) -> Self {
        match value {
            0 => Self::Unknown,
            1 => Self::RegularFile,
            2 => Self::Directory,
            3 => Self::CharacterDevice,
            4 => Self::BlockDevice,
            5 => Self::Fifo,
            6 => Self::Socket,
            7 => Self::Symlink,
            _ => Self::Unknown,
        }
    }

    pub(crate) const fn to_raw(self) -> u8 {
        match self {
            Self::Unknown => 0,
            Self::RegularFile => 1,
            Self::Directory => 2,
            Self::CharacterDevice => 3,
            Self::BlockDevice => 4,
            Self::Fifo => 5,
            Self::Socket => 6,
            Self::Symlink => 7,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RawDirectoryEntry {
    inode: u32,
    rec_len: u16,
    name_len: u8,
    file_type: u8,
}

impl RawDirectoryEntry {
    pub(crate) fn decode(input: &[u8], offset: usize) -> Ext4Result<Self> {
        Ok(Self {
            inode: codec::le_u32(input, offset)?,
            rec_len: codec::le_u16(input, offset + 0x04)?,
            name_len: *input
                .get(offset + 0x06)
                .ok_or(crate::Ext4Error::Corrupt(crate::CorruptKind::Truncated))?,
            file_type: *input
                .get(offset + 0x07)
                .ok_or(crate::Ext4Error::Corrupt(crate::CorruptKind::Truncated))?,
        })
    }

    pub(crate) const fn inode(self) -> u32 {
        self.inode
    }

    pub(crate) fn record_len(self, block_size: usize) -> Ext4Result<usize> {
        let raw = usize::from(self.rec_len);
        // Linux ext4 treats both special disk values as an entire directory block.
        if raw == u16::MAX as usize || raw == 0 {
            return Ok(block_size);
        }
        let high_bits = raw.checked_shl(16).ok_or(Ext4Error::Overflow)? & 0x30000;
        Ok((raw & 0xfffc) | high_bits)
    }

    pub(crate) fn encode_record_len(len: usize, block_size: usize) -> Ext4Result<u16> {
        if len == 0 || !len.is_multiple_of(4) || len > block_size {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }
        if len == block_size && block_size > u16::MAX as usize {
            return Ok(u16::MAX);
        }
        if block_size > u16::MAX as usize {
            let encoded = (len & 0xfffc) | ((len >> 16) & 0x3);
            return u16::try_from(encoded).map_err(|_| Ext4Error::Overflow);
        }
        u16::try_from(len).map_err(|_| Ext4Error::Overflow)
    }

    pub(crate) const fn name_len(self) -> u8 {
        self.name_len
    }

    pub(crate) fn file_type(self) -> DirectoryFileType {
        DirectoryFileType::from_raw(self.file_type)
    }

    pub(crate) const fn is_checksum_tail(self) -> bool {
        self.inode == 0
            && self.rec_len == DIRENT_TAIL_SIZE as u16
            && self.name_len == 0
            && self.file_type == DIRENT_TAIL_FILE_TYPE
    }
}

pub(crate) fn tail_checksum(input: &[u8]) -> Ext4Result<u32> {
    let offset = input
        .len()
        .checked_sub(4)
        .ok_or(crate::Ext4Error::Corrupt(crate::CorruptKind::Truncated))?;
    codec::le_u32(input, offset)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HTreeRootInfo {
    reserved_zero: u32,
    hash_version: u8,
    info_length: u8,
    indirect_levels: u8,
    flags: u8,
}

impl HTreeRootInfo {
    pub(crate) fn decode(input: &[u8]) -> Ext4Result<Self> {
        Ok(Self {
            reserved_zero: codec::le_u32(input, DX_ROOT_INFO_OFFSET)?,
            hash_version: *input
                .get(DX_ROOT_INFO_OFFSET + 4)
                .ok_or(crate::Ext4Error::Corrupt(crate::CorruptKind::Truncated))?,
            info_length: *input
                .get(DX_ROOT_INFO_OFFSET + 5)
                .ok_or(crate::Ext4Error::Corrupt(crate::CorruptKind::Truncated))?,
            indirect_levels: *input
                .get(DX_ROOT_INFO_OFFSET + 6)
                .ok_or(crate::Ext4Error::Corrupt(crate::CorruptKind::Truncated))?,
            flags: *input
                .get(DX_ROOT_INFO_OFFSET + 7)
                .ok_or(crate::Ext4Error::Corrupt(crate::CorruptKind::Truncated))?,
        })
    }

    pub(crate) const fn indirect_levels(self) -> u8 {
        self.indirect_levels
    }

    pub(crate) const fn hash_version(self) -> u8 {
        self.hash_version
    }

    pub(crate) fn validate(self) -> Ext4Result<()> {
        if self.reserved_zero != 0
            || self.info_length != 8
            || self.flags & 1 != 0
            || !matches!(
                self.hash_version,
                DX_HASH_LEGACY | DX_HASH_HALF_MD4 | DX_HASH_TEA | DX_HASH_SIPHASH
            )
        {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HTreeCountLimit {
    limit: u16,
    count: u16,
}

impl HTreeCountLimit {
    pub(crate) fn decode(input: &[u8], offset: usize) -> Ext4Result<Self> {
        Ok(Self {
            limit: codec::le_u16(input, offset)?,
            count: codec::le_u16(input, offset + 2)?,
        })
    }

    pub(crate) const fn limit(self) -> u16 {
        self.limit
    }

    pub(crate) const fn count(self) -> u16 {
        self.count
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HTreeEntry {
    hash: u32,
    block: u32,
}

impl HTreeEntry {
    pub(crate) fn decode(input: &[u8], offset: usize) -> Ext4Result<Self> {
        Ok(Self {
            hash: codec::le_u32(input, offset)?,
            block: codec::le_u32(input, offset + 4)?,
        })
    }

    pub(crate) fn decode_indexed(
        input: &[u8],
        count_offset: usize,
        index: usize,
    ) -> Ext4Result<Self> {
        if index == 0 {
            return Ok(Self {
                hash: 0,
                block: codec::le_u32(input, count_offset + DX_COUNT_LIMIT_SIZE)?,
            });
        }
        let offset = count_offset
            .checked_add(
                index
                    .checked_mul(DX_ENTRY_SIZE)
                    .ok_or(crate::Ext4Error::Overflow)?,
            )
            .ok_or(crate::Ext4Error::Overflow)?;
        Self::decode(input, offset)
    }

    pub(crate) const fn hash(self) -> u32 {
        self.hash
    }

    pub(crate) const fn block(self) -> u32 {
        self.block & DX_BLOCK_MASK
    }
}

#[cfg(unittest)]
mod unittests {
    use unittest::{assert_eq, def_test};

    use super::*;

    #[def_test]
    fn special_record_lengths_follow_linux_encoding() {
        let zero = raw_directory_entry(0);
        let maximum = raw_directory_entry(u16::MAX);

        assert_eq!(zero.record_len(4_096), Ok(4_096));
        assert_eq!(maximum.record_len(4_096), Ok(4_096));
        assert_eq!(
            RawDirectoryEntry::encode_record_len(4_096, 4_096),
            Ok(4_096)
        );
        assert_eq!(
            RawDirectoryEntry::encode_record_len(65_536, 65_536),
            Ok(u16::MAX)
        );
    }

    #[def_test]
    fn htree_root_distinguishes_disk_and_derived_hash_versions() {
        let mut bytes = [0; DX_ROOT_INFO_OFFSET + 8];
        bytes[DX_ROOT_INFO_OFFSET + 4] = DX_HASH_SIPHASH;
        bytes[DX_ROOT_INFO_OFFSET + 5] = 8;
        let root_info = HTreeRootInfo::decode(&bytes).expect("decode htree root info");
        assert_eq!(root_info.validate(), Ok(()));

        bytes[DX_ROOT_INFO_OFFSET + 4] = 5;
        let root_info = HTreeRootInfo::decode(&bytes).expect("decode htree root info");
        assert_eq!(
            root_info.validate(),
            Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry))
        );
    }

    fn raw_directory_entry(rec_len: u16) -> RawDirectoryEntry {
        let mut bytes = [0; DIRENT_HEADER_SIZE];
        bytes[4..6].copy_from_slice(&rec_len.to_le_bytes());
        RawDirectoryEntry::decode(&bytes, 0).expect("decode directory entry")
    }
}

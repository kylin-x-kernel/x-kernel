// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use crate::{Ext4Result, disk::codec};

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
pub(crate) const DX_MAX_TREE_DEPTH_WITHOUT_LARGEDIR: u8 = 2;
pub(crate) const DX_MAX_TREE_DEPTH_WITH_LARGEDIR: u8 = 3;

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

    pub(crate) const fn rec_len(self) -> u16 {
        self.rec_len
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

    pub(crate) const fn reserved_zero(self) -> u32 {
        self.reserved_zero
    }

    pub(crate) const fn info_length(self) -> u8 {
        self.info_length
    }

    pub(crate) const fn indirect_levels(self) -> u8 {
        self.indirect_levels
    }

    pub(crate) const fn hash_version(self) -> u8 {
        self.hash_version
    }

    pub(crate) const fn flags(self) -> u8 {
        self.flags
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

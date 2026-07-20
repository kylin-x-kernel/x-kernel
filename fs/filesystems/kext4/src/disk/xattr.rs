// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use crate::{Ext4Result, disk::codec};

pub(crate) const XATTR_MAGIC: u32 = 0xea02_0000;
pub(crate) const XATTR_HEADER_SIZE: usize = 32;
pub(crate) const XATTR_IBODY_HEADER_SIZE: usize = 4;
pub(crate) const XATTR_ENTRY_HEADER_SIZE: usize = 16;
pub(crate) const XATTR_PAD: usize = 4;
pub(crate) const XATTR_BLOCK_CHECKSUM_OFFSET: usize = 16;
pub(crate) const XATTR_REFCOUNT_MAX: u32 = 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct XattrBlockHeader {
    magic: u32,
    refcount: u32,
    blocks: u32,
    checksum: u32,
    reserved: [u32; 3],
}

impl XattrBlockHeader {
    pub(crate) fn decode(input: &[u8]) -> Ext4Result<Self> {
        let _hash = codec::le_u32(input, 0x0c)?;
        Ok(Self {
            magic: codec::le_u32(input, 0x00)?,
            refcount: codec::le_u32(input, 0x04)?,
            blocks: codec::le_u32(input, 0x08)?,
            checksum: codec::le_u32(input, 0x10)?,
            reserved: [
                codec::le_u32(input, 0x14)?,
                codec::le_u32(input, 0x18)?,
                codec::le_u32(input, 0x1c)?,
            ],
        })
    }

    pub(crate) const fn magic(self) -> u32 {
        self.magic
    }

    pub(crate) const fn refcount(self) -> u32 {
        self.refcount
    }

    pub(crate) const fn blocks(self) -> u32 {
        self.blocks
    }

    pub(crate) const fn checksum(self) -> u32 {
        self.checksum
    }

    pub(crate) const fn reserved(self) -> [u32; 3] {
        self.reserved
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct XattrEntryHeader {
    name_len: u8,
    name_index: u8,
    value_offs: u16,
    value_inum: u32,
    value_size: u32,
}

impl XattrEntryHeader {
    pub(crate) fn decode(input: &[u8], offset: usize) -> Ext4Result<Self> {
        let _hash = codec::le_u32(input, offset + 0x0c)?;
        Ok(Self {
            name_len: *input
                .get(offset)
                .ok_or(crate::Ext4Error::Corrupt(crate::CorruptKind::Truncated))?,
            name_index: *input
                .get(offset + 1)
                .ok_or(crate::Ext4Error::Corrupt(crate::CorruptKind::Truncated))?,
            value_offs: codec::le_u16(input, offset + 0x02)?,
            value_inum: codec::le_u32(input, offset + 0x04)?,
            value_size: codec::le_u32(input, offset + 0x08)?,
        })
    }

    pub(crate) const fn name_len(self) -> u8 {
        self.name_len
    }

    pub(crate) const fn name_index(self) -> u8 {
        self.name_index
    }

    pub(crate) const fn value_offs(self) -> u16 {
        self.value_offs
    }

    pub(crate) const fn value_inum(self) -> u32 {
        self.value_inum
    }

    pub(crate) const fn value_size(self) -> u32 {
        self.value_size
    }
}

pub(crate) fn padded_len(len: usize) -> Ext4Result<usize> {
    len.checked_add(XATTR_PAD - 1)
        .map(|len| len & !(XATTR_PAD - 1))
        .ok_or(crate::Ext4Error::Overflow)
}

pub(crate) fn entry_len(name_len: usize) -> Ext4Result<usize> {
    padded_len(
        XATTR_ENTRY_HEADER_SIZE
            .checked_add(name_len)
            .ok_or(crate::Ext4Error::Overflow)?,
    )
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use crate::{Ext4Result, PhysicalBlock, disk::codec};

pub(crate) const EXTENT_HEADER_SIZE: usize = 12;
pub(crate) const EXTENT_ENTRY_SIZE: usize = 12;
pub(crate) const EXTENT_MAGIC: u16 = 0xf30a;
pub(crate) const EXTENT_MAX_DEPTH: u16 = 5;
pub(crate) const EXT_INIT_MAX_LEN: u16 = 0x8000;
pub(crate) const EXT_UNWRITTEN_FLAG: u16 = 0x8000;
pub(crate) const EXT_UNWRITTEN_MAX_LEN: u16 = 0x7fff;

pub(crate) fn encode_empty_root(output: &mut [u8]) -> Ext4Result<()> {
    if output.len() < EXTENT_HEADER_SIZE {
        return Err(crate::Ext4Error::Corrupt(crate::CorruptKind::Truncated));
    }
    output.fill(0);
    output[0x00..0x02].copy_from_slice(&EXTENT_MAGIC.to_le_bytes());
    output[0x04..0x06].copy_from_slice(&inline_extent_capacity()?.to_le_bytes());
    Ok(())
}

pub(crate) fn encode_header(
    output: &mut [u8],
    entries: u16,
    max: u16,
    depth: u16,
    generation: u32,
) -> Ext4Result<()> {
    if output.len() < EXTENT_HEADER_SIZE {
        return Err(crate::Ext4Error::Corrupt(crate::CorruptKind::Truncated));
    }
    put_u16(output, 0x00, EXTENT_MAGIC)?;
    put_u16(output, 0x02, entries)?;
    put_u16(output, 0x04, max)?;
    put_u16(output, 0x06, depth)?;
    put_u32(output, 0x08, generation)
}

pub(crate) fn update_header_entries(output: &mut [u8], entries: u16) -> Ext4Result<()> {
    put_u16(output, 0x02, entries)
}

pub(crate) fn inline_extent_capacity() -> Ext4Result<u16> {
    let entry_bytes = crate::disk::inode::INODE_BLOCK_BYTES
        .checked_sub(EXTENT_HEADER_SIZE)
        .ok_or(crate::Ext4Error::Overflow)?;
    u16::try_from(entry_bytes / EXTENT_ENTRY_SIZE).map_err(|_| crate::Ext4Error::Overflow)
}

pub(crate) fn extent_block_capacity(block_size: usize) -> Ext4Result<u16> {
    let entry_bytes = block_size
        .checked_sub(EXTENT_HEADER_SIZE)
        .ok_or(crate::Ext4Error::Overflow)?;
    u16::try_from(entry_bytes / EXTENT_ENTRY_SIZE).map_err(|_| crate::Ext4Error::Overflow)
}

pub(crate) fn tail_offset(header: ExtentHeader) -> Ext4Result<usize> {
    usize::from(header.max())
        .checked_mul(EXTENT_ENTRY_SIZE)
        .and_then(|offset| offset.checked_add(EXTENT_HEADER_SIZE))
        .ok_or(crate::Ext4Error::Overflow)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ExtentHeader {
    entries: u16,
    max: u16,
    depth: u16,
    generation: u32,
}

impl ExtentHeader {
    pub(crate) fn decode(input: &[u8]) -> Ext4Result<Self> {
        Ok(Self {
            entries: codec::le_u16(input, 0x02)?,
            max: codec::le_u16(input, 0x04)?,
            depth: codec::le_u16(input, 0x06)?,
            generation: codec::le_u32(input, 0x08)?,
        })
    }

    pub(crate) fn validate(&self) -> bool {
        self.entries <= self.max && self.depth <= EXTENT_MAX_DEPTH
    }

    pub(crate) fn has_magic(input: &[u8]) -> Ext4Result<bool> {
        Ok(codec::le_u16(input, 0x00)? == EXTENT_MAGIC)
    }

    pub(crate) const fn entries(self) -> u16 {
        self.entries
    }

    pub(crate) const fn max(self) -> u16 {
        self.max
    }

    pub(crate) const fn depth(self) -> u16 {
        self.depth
    }

    pub(crate) const fn generation(self) -> u32 {
        self.generation
    }
}

pub(crate) fn tail_checksum(input: &[u8], header: ExtentHeader) -> Ext4Result<u32> {
    codec::le_u32(input, tail_offset(header)?)
}

pub(crate) fn write_tail_checksum(
    output: &mut [u8],
    header: ExtentHeader,
    checksum: u32,
) -> Ext4Result<()> {
    put_u32(output, tail_offset(header)?, checksum)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ExtentIndex {
    block: u32,
    leaf: PhysicalBlock,
}

impl ExtentIndex {
    pub(crate) const fn new(block: u32, leaf: PhysicalBlock) -> Self {
        Self { block, leaf }
    }

    pub(crate) fn decode(input: &[u8]) -> Ext4Result<Self> {
        let leaf_lo = u64::from(codec::le_u32(input, 0x04)?);
        let leaf_hi = u64::from(codec::le_u16(input, 0x08)?);
        Ok(Self {
            block: codec::le_u32(input, 0x00)?,
            leaf: PhysicalBlock::new(leaf_lo | (leaf_hi << 32)),
        })
    }

    pub(crate) const fn block(self) -> u32 {
        self.block
    }

    pub(crate) const fn leaf(self) -> PhysicalBlock {
        self.leaf
    }

    pub(crate) fn encode(self, output: &mut [u8]) -> Ext4Result<()> {
        put_u32(output, 0x00, self.block)?;
        put_u32(output, 0x04, self.leaf.get() as u32)?;
        put_u16(output, 0x08, (self.leaf.get() >> 32) as u16)?;
        put_u16(output, 0x0a, 0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ExtentLeaf {
    block: u32,
    len: u16,
    start: PhysicalBlock,
}

impl ExtentLeaf {
    pub(crate) const fn new(block: u32, encoded_len: u16, start: PhysicalBlock) -> Self {
        Self {
            block,
            len: encoded_len,
            start,
        }
    }

    pub(crate) fn decode(input: &[u8]) -> Ext4Result<Self> {
        let start_hi = u64::from(codec::le_u16(input, 0x06)?);
        let start_lo = u64::from(codec::le_u32(input, 0x08)?);
        Ok(Self {
            block: codec::le_u32(input, 0x00)?,
            len: codec::le_u16(input, 0x04)?,
            start: PhysicalBlock::new(start_lo | (start_hi << 32)),
        })
    }

    pub(crate) const fn block(self) -> u32 {
        self.block
    }

    pub(crate) const fn start(self) -> PhysicalBlock {
        self.start
    }

    pub(crate) const fn is_unwritten(self) -> bool {
        self.len > EXT_INIT_MAX_LEN
    }

    pub(crate) const fn actual_len(self) -> u16 {
        if self.len <= EXT_INIT_MAX_LEN {
            self.len
        } else {
            self.len - EXT_INIT_MAX_LEN
        }
    }

    pub(crate) fn encode(self, output: &mut [u8]) -> Ext4Result<()> {
        put_u32(output, 0x00, self.block)?;
        put_u16(output, 0x04, self.len)?;
        put_u16(output, 0x06, (self.start.get() >> 32) as u16)?;
        put_u32(output, 0x08, self.start.get() as u32)
    }
}

fn put_u16(output: &mut [u8], offset: usize, value: u16) -> Ext4Result<()> {
    let end = offset.checked_add(2).ok_or(crate::Ext4Error::Overflow)?;
    output
        .get_mut(offset..end)
        .ok_or(crate::Ext4Error::Corrupt(crate::CorruptKind::Truncated))?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn put_u32(output: &mut [u8], offset: usize, value: u32) -> Ext4Result<()> {
    let end = offset.checked_add(4).ok_or(crate::Ext4Error::Overflow)?;
    output
        .get_mut(offset..end)
        .ok_or(crate::Ext4Error::Corrupt(crate::CorruptKind::Truncated))?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

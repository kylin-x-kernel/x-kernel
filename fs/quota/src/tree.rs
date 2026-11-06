// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Four-level v2 quota radix tree and on-disk free lists.

use kerrno::KResult;

use crate::{
    Dqblk, QuotaFormat, QuotaIo, corrupt, overflow, read_exact,
    v2::{decode_record, encode_record, put_u32, u32_at},
    write_exact,
};

const BLOCK_BYTES: usize = 1024;
const DATA_HEADER_BYTES: usize = 16;
const ROOT_BLOCK: u32 = 1;
const TREE_DEPTH: usize = 4;
pub(crate) const fn record_growth_bytes_bound() -> u64 {
    (TREE_DEPTH * BLOCK_BYTES) as u64
}

pub(crate) const fn record_write_blocks_bound() -> u32 {
    // Tree path, data block, two free-entry neighbors, info, and one spare.
    TREE_DEPTH as u32 + 5
}

type Block = [u8; BLOCK_BYTES];

#[derive(Debug)]
pub(crate) struct QuotaTree {
    pub(crate) format: QuotaFormat,
    pub(crate) blocks: u32,
    pub(crate) free_block: u32,
    pub(crate) free_entry: u32,
    pub(crate) is_dirty: bool,
}

impl QuotaTree {
    pub(crate) fn new(
        format: QuotaFormat,
        blocks: u32,
        free_block: u32,
        free_entry: u32,
        file_bytes: u64,
    ) -> KResult<Self> {
        if blocks <= ROOT_BLOCK
            || u64::from(blocks) * BLOCK_BYTES as u64 > file_bytes
            || (free_block != 0 && free_block == free_entry)
        {
            return Err(corrupt());
        }
        let tree = Self {
            format,
            blocks,
            free_block,
            free_entry,
            is_dirty: false,
        };
        tree.check_optional_block(free_block)?;
        tree.check_optional_block(free_entry)?;
        Ok(tree)
    }

    pub(crate) fn read<F>(
        &self,
        io: &mut dyn QuotaIo<File = F>,
        file: &F,
        id: u32,
    ) -> KResult<(u64, Dqblk)> {
        let mut block_number = ROOT_BLOCK;
        let mut path = [ROOT_BLOCK; TREE_DEPTH + 1];
        for depth in 0..TREE_DEPTH {
            let block = self.read_block(io, file, block_number)?;
            let child = u32_at(&block, tree_index(id, depth) * 4);
            if child == 0 {
                return Ok((0, Dqblk::default()));
            }
            self.check_child(child, &path[..=depth])?;
            path[depth + 1] = child;
            block_number = child;
        }
        let block = self.read_block(io, file, block_number)?;
        self.check_data_header(&block)?;
        let entry_size = self.format.entry_size();
        let mut found = None;
        for index in 0..entries_per_block(self.format) {
            let start = DATA_HEADER_BYTES + index * entry_size;
            let entry = &block[start..start + entry_size];
            if entry.iter().any(|&byte| byte != 0) && u32_at(entry, 0) == id {
                if found.is_some() {
                    return Err(corrupt());
                }
                found = Some((
                    block_offset(block_number) + start as u64,
                    decode_record(self.format, entry)?,
                ));
            }
        }
        found.ok_or_else(corrupt)
    }

    pub(crate) fn write<F>(
        &mut self,
        io: &mut dyn QuotaIo<File = F>,
        file: &F,
        id: u32,
        offset: &mut u64,
        data: Dqblk,
    ) -> KResult<()> {
        let mut entry = [0; 72];
        encode_record(
            self.format,
            id,
            data,
            &mut entry[..self.format.entry_size()],
        )?;
        if *offset != 0 {
            return write_exact(io, file, *offset, &entry[..self.format.entry_size()]);
        }
        let mut path = [ROOT_BLOCK; TREE_DEPTH + 1];
        let new_offset = self.insert_tree(
            io,
            file,
            id,
            &entry[..self.format.entry_size()],
            &mut path,
            0,
        )?;
        *offset = new_offset;
        Ok(())
    }

    pub(crate) fn delete<F>(
        &mut self,
        io: &mut dyn QuotaIo<File = F>,
        file: &F,
        id: u32,
        offset: &mut u64,
    ) -> KResult<()> {
        if *offset == 0 {
            return Ok(());
        }
        let mut path = [ROOT_BLOCK; TREE_DEPTH + 1];
        self.remove_tree(io, file, id, offset, &mut path, 0)?;
        Ok(())
    }

    pub(crate) fn next_id<F>(
        &self,
        io: &mut dyn QuotaIo<File = F>,
        file: &F,
        minimum: u32,
    ) -> KResult<Option<u32>> {
        let mut path = [ROOT_BLOCK; TREE_DEPTH + 1];
        self.next_in_tree(io, file, minimum, 0, &mut path, 0)
    }

    fn check_optional_block(&self, block: u32) -> KResult<()> {
        if block != 0 && (block <= ROOT_BLOCK || block >= self.blocks) {
            return Err(corrupt());
        }
        Ok(())
    }

    fn check_child(&self, child: u32, path: &[u32]) -> KResult<()> {
        self.check_optional_block(child)?;
        if child == 0 || path.contains(&child) {
            return Err(corrupt());
        }
        Ok(())
    }

    fn read_block<F>(
        &self,
        io: &mut dyn QuotaIo<File = F>,
        file: &F,
        block: u32,
    ) -> KResult<Block> {
        if block == 0 || block >= self.blocks {
            return Err(corrupt());
        }
        let mut bytes = [0; BLOCK_BYTES];
        read_exact(io, file, block_offset(block), &mut bytes)?;
        Ok(bytes)
    }

    fn write_block<F>(
        &self,
        io: &mut dyn QuotaIo<File = F>,
        file: &F,
        block: u32,
        bytes: &Block,
    ) -> KResult<()> {
        write_exact(io, file, block_offset(block), bytes)
    }

    fn check_data_header(&self, block: &Block) -> KResult<()> {
        self.check_optional_block(u32_at(block, 0))?;
        self.check_optional_block(u32_at(block, 4))?;
        let entries = data_count(block);
        let actual = block[DATA_HEADER_BYTES..]
            .chunks_exact(self.format.entry_size())
            .filter(|entry| entry.iter().any(|&byte| byte != 0))
            .count();
        if usize::from(entries) > entries_per_block(self.format) || usize::from(entries) != actual {
            return Err(corrupt());
        }
        Ok(())
    }

    fn allocate_block<F>(
        &mut self,
        io: &mut dyn QuotaIo<File = F>,
        file: &F,
        path: &[u32],
    ) -> KResult<u32> {
        let number = if self.free_block != 0 {
            self.free_block
        } else {
            self.blocks
        };
        if path.contains(&number) {
            return Err(corrupt());
        }
        if self.free_block != 0 {
            let block = self.read_block(io, file, number)?;
            let next = u32_at(&block, 0);
            self.check_optional_block(next)?;
            if next == number
                || path.contains(&next)
                || u32_at(&block, 4) != 0
                || data_count(&block) != 0
            {
                return Err(corrupt());
            }
            self.write_block(io, file, number, &[0; BLOCK_BYTES])?;
            self.free_block = next;
        } else {
            let blocks = self.blocks.checked_add(1).ok_or_else(overflow)?;
            self.write_block(io, file, number, &[0; BLOCK_BYTES])?;
            self.blocks = blocks;
        }
        self.is_dirty = true;
        Ok(number)
    }

    fn free_block<F>(
        &mut self,
        io: &mut dyn QuotaIo<File = F>,
        file: &F,
        number: u32,
    ) -> KResult<()> {
        self.check_child(number, &[ROOT_BLOCK, self.free_block])?;
        let mut block = [0; BLOCK_BYTES];
        put_u32(&mut block, 0, self.free_block);
        self.write_block(io, file, number, &block)?;
        self.free_block = number;
        self.is_dirty = true;
        Ok(())
    }

    fn unlink_free_entry<F>(
        &mut self,
        io: &mut dyn QuotaIo<File = F>,
        file: &F,
        number: u32,
        block: &mut Block,
    ) -> KResult<Option<u32>> {
        let next = u32_at(block, 0);
        let previous = u32_at(block, 4);
        self.check_optional_block(next)?;
        self.check_optional_block(previous)?;
        if next == number || previous == number || ((previous == 0) != (self.free_entry == number))
        {
            return Err(corrupt());
        }
        let mut next_block = if next != 0 {
            Some(self.read_block(io, file, next)?)
        } else {
            None
        };
        let mut previous_block = if previous != 0 {
            Some(self.read_block(io, file, previous)?)
        } else {
            None
        };
        if next_block
            .as_ref()
            .is_some_and(|bytes| u32_at(bytes, 4) != number)
            || previous_block
                .as_ref()
                .is_some_and(|bytes| u32_at(bytes, 0) != number)
        {
            return Err(corrupt());
        }
        if let Some(bytes) = &mut next_block {
            put_u32(bytes, 4, previous);
            self.write_block(io, file, next, bytes)?;
        }
        if let Some(bytes) = &mut previous_block {
            put_u32(bytes, 0, next);
            self.write_block(io, file, previous, bytes)?;
        }
        put_u32(block, 0, 0);
        put_u32(block, 4, 0);
        // The caller publishes this head only after its target write succeeds.
        Ok((previous == 0).then_some(next))
    }

    fn link_free_entry<F>(
        &mut self,
        io: &mut dyn QuotaIo<File = F>,
        file: &F,
        number: u32,
        block: &mut Block,
    ) -> KResult<()> {
        if self.free_entry == number {
            return Err(corrupt());
        }
        let mut old_head = if self.free_entry != 0 {
            let bytes = self.read_block(io, file, self.free_entry)?;
            if u32_at(&bytes, 4) != 0 {
                return Err(corrupt());
            }
            Some(bytes)
        } else {
            None
        };
        put_u32(block, 0, self.free_entry);
        put_u32(block, 4, 0);
        self.write_block(io, file, number, block)?;
        if let Some(bytes) = &mut old_head {
            put_u32(bytes, 4, number);
            self.write_block(io, file, self.free_entry, bytes)?;
        }
        self.free_entry = number;
        self.is_dirty = true;
        Ok(())
    }

    fn insert_entry<F>(
        &mut self,
        io: &mut dyn QuotaIo<File = F>,
        file: &F,
        entry: &[u8],
        path: &[u32],
    ) -> KResult<(u32, u64)> {
        let number;
        let mut block;
        if self.free_entry != 0 {
            number = self.free_entry;
            self.check_child(number, path)?;
            block = self.read_block(io, file, number)?;
            self.check_data_header(&block)?;
            if u32_at(&block, 4) != 0 {
                return Err(corrupt());
            }
        } else {
            number = self.allocate_block(io, file, path)?;
            block = [0; BLOCK_BYTES];
            self.free_entry = number;
            self.is_dirty = true;
        }
        let slot = (0..entries_per_block(self.format))
            .find(|&index| {
                let start = DATA_HEADER_BYTES + index * self.format.entry_size();
                block[start..start + self.format.entry_size()]
                    .iter()
                    .all(|&byte| byte == 0)
            })
            .ok_or_else(corrupt)?;
        let count = data_count(&block) + 1;
        let new_head = if usize::from(count) == entries_per_block(self.format) {
            self.unlink_free_entry(io, file, number, &mut block)?
        } else {
            None
        };
        set_data_count(&mut block, count);
        let start = DATA_HEADER_BYTES + slot * self.format.entry_size();
        block[start..start + entry.len()].copy_from_slice(entry);
        self.write_block(io, file, number, &block)?;
        if let Some(head) = new_head {
            self.free_entry = head;
            self.is_dirty = true;
        }
        Ok((number, block_offset(number) + start as u64))
    }

    fn insert_tree<F>(
        &mut self,
        io: &mut dyn QuotaIo<File = F>,
        file: &F,
        id: u32,
        entry: &[u8],
        path: &mut [u32; TREE_DEPTH + 1],
        depth: usize,
    ) -> KResult<u64> {
        let mut block = self.read_block(io, file, path[depth])?;
        let index = tree_index(id, depth) * 4;
        let existing = u32_at(&block, index);
        if existing != 0 {
            self.check_child(existing, &path[..=depth])?;
        }
        let child;
        let offset;
        if depth == TREE_DEPTH - 1 {
            if existing != 0 {
                return Err(corrupt());
            }
            (child, offset) = self.insert_entry(io, file, entry, &path[..=depth])?;
        } else {
            child = if existing == 0 {
                self.allocate_block(io, file, &path[..=depth])?
            } else {
                existing
            };
            path[depth + 1] = child;
            offset = self.insert_tree(io, file, id, entry, path, depth + 1)?;
        }
        if existing == 0 {
            put_u32(&mut block, index, child);
            self.write_block(io, file, path[depth], &block)?;
        }
        Ok(offset)
    }

    fn free_entry<F>(
        &mut self,
        io: &mut dyn QuotaIo<File = F>,
        file: &F,
        id: u32,
        number: u32,
        offset: u64,
    ) -> KResult<()> {
        let mut block = self.read_block(io, file, number)?;
        self.check_data_header(&block)?;
        if offset / BLOCK_BYTES as u64 != u64::from(number) {
            return Err(corrupt());
        }
        let start = (offset % BLOCK_BYTES as u64) as usize;
        if start < DATA_HEADER_BYTES
            || !(start - DATA_HEADER_BYTES).is_multiple_of(self.format.entry_size())
            || start + self.format.entry_size() > BLOCK_BYTES
            || data_count(&block) == 0
            || u32_at(&block, start) != id
            || block[start..start + self.format.entry_size()]
                .iter()
                .all(|&byte| byte == 0)
        {
            return Err(corrupt());
        }
        let previous_count = data_count(&block);
        set_data_count(&mut block, previous_count - 1);
        block[start..start + self.format.entry_size()].fill(0);
        if previous_count == 1 {
            let new_head = self.unlink_free_entry(io, file, number, &mut block)?;
            self.free_block(io, file, number)?;
            if let Some(head) = new_head {
                self.free_entry = head;
                self.is_dirty = true;
            }
            Ok(())
        } else if usize::from(previous_count) == entries_per_block(self.format) {
            self.link_free_entry(io, file, number, &mut block)
        } else {
            self.write_block(io, file, number, &block)
        }
    }

    fn remove_tree<F>(
        &mut self,
        io: &mut dyn QuotaIo<File = F>,
        file: &F,
        id: u32,
        offset: &mut u64,
        path: &mut [u32; TREE_DEPTH + 1],
        depth: usize,
    ) -> KResult<bool> {
        let number = path[depth];
        let mut block = self.read_block(io, file, number)?;
        let index = tree_index(id, depth) * 4;
        let child = u32_at(&block, index);
        self.check_child(child, &path[..=depth])?;
        path[depth + 1] = child;
        let should_remove_child = if depth == TREE_DEPTH - 1 {
            self.free_entry(io, file, id, child, *offset)?;
            *offset = 0;
            true
        } else {
            self.remove_tree(io, file, id, offset, path, depth + 1)?
        };
        if !should_remove_child {
            return Ok(false);
        }
        put_u32(&mut block, index, 0);
        if number != ROOT_BLOCK && block.iter().all(|&byte| byte == 0) {
            self.free_block(io, file, number)?;
            Ok(true)
        } else {
            self.write_block(io, file, number, &block)?;
            Ok(false)
        }
    }

    fn next_in_tree<F>(
        &self,
        io: &mut dyn QuotaIo<File = F>,
        file: &F,
        minimum: u32,
        prefix: u32,
        path: &mut [u32; TREE_DEPTH + 1],
        depth: usize,
    ) -> KResult<Option<u32>> {
        let block = self.read_block(io, file, path[depth])?;
        let shift = (TREE_DEPTH - depth - 1) * 8;
        for index in 0..256 {
            let candidate = prefix | ((index as u32) << shift);
            let last = u64::from(candidate) + (1_u64 << shift) - 1;
            if last < u64::from(minimum) {
                continue;
            }
            let child = u32_at(&block, index * 4);
            if child == 0 {
                continue;
            }
            self.check_child(child, &path[..=depth])?;
            if depth == TREE_DEPTH - 1 {
                if candidate == u32::MAX {
                    return Err(corrupt());
                }
                return Ok(Some(candidate));
            }
            path[depth + 1] = child;
            if let Some(id) = self.next_in_tree(io, file, minimum, candidate, path, depth + 1)? {
                return Ok(Some(id));
            }
        }
        Ok(None)
    }
}

fn tree_index(id: u32, depth: usize) -> usize {
    ((id >> ((TREE_DEPTH - depth - 1) * 8)) & 255) as usize
}

fn block_offset(number: u32) -> u64 {
    u64::from(number) * BLOCK_BYTES as u64
}

fn entries_per_block(format: QuotaFormat) -> usize {
    (BLOCK_BYTES - DATA_HEADER_BYTES) / format.entry_size()
}

fn data_count(block: &Block) -> u16 {
    u16::from_le_bytes([block[8], block[9]])
}

fn set_data_count(block: &mut Block, value: u16) {
    block[8..10].copy_from_slice(&value.to_le_bytes());
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{vec, vec::Vec};
use core::str;

use crate::{
    BlockMapping, ChecksumTarget, CorruptKind, Ext4Error, Ext4Result, Ext4SbInfo, FilesystemBlock,
    InodeNumber, LogicalBlock, UnsupportedKind,
    disk::{DirectoryFileType, checksum, codec, dir as disk_dir},
    inode::{Ext4Inode, InodeKind, inode_checksum_seed},
};

/// Directory byte position in an ext4 directory file.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct Ext4DirPos(u64);

impl Ext4DirPos {
    /// Creates a directory position from a raw directory-file byte offset.
    ///
    /// `read_dir_from` accepts offset zero, the logical end position, and
    /// positions previously returned by directory streaming. Other offsets may
    /// be rejected when they do not point at an ext4 directory record boundary.
    pub const fn new(offset: u64) -> Self {
        Self(offset)
    }

    /// Returns the raw directory-file byte offset.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// A decoded ext4 directory entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectoryEntry {
    name: Vec<u8>,
    inode: InodeNumber,
    file_type: DirectoryFileType,
    offset: u64,
}

impl DirectoryEntry {
    fn from_scanned(entry: ScannedDirectoryEntry<'_>) -> Self {
        Self {
            name: Vec::from(entry.name),
            inode: entry.inode,
            file_type: entry.file_type,
            offset: entry.offset,
        }
    }

    /// Returns the raw entry name bytes.
    pub fn name_bytes(&self) -> &[u8] {
        &self.name
    }

    /// Returns the entry name as UTF-8 when it is valid UTF-8.
    pub fn name(&self) -> Option<&str> {
        str::from_utf8(&self.name).ok()
    }

    /// Returns the target inode number.
    pub const fn inode(&self) -> InodeNumber {
        self.inode
    }

    /// Returns the ext4 directory file type.
    pub const fn file_type(&self) -> DirectoryFileType {
        self.file_type
    }

    /// Returns the byte offset of this record in the directory file.
    pub const fn offset(&self) -> u64 {
        self.offset
    }
}

/// Borrowed ext4 directory entry emitted while scanning a directory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Ext4DirEntryRef<'a> {
    name: &'a [u8],
    inode: InodeNumber,
    file_type: DirectoryFileType,
    offset: Ext4DirPos,
}

impl<'a> Ext4DirEntryRef<'a> {
    /// Returns the raw entry name bytes.
    pub const fn name_bytes(self) -> &'a [u8] {
        self.name
    }

    /// Returns the entry name as UTF-8 when it is valid UTF-8.
    pub fn name(self) -> Option<&'a str> {
        str::from_utf8(self.name).ok()
    }

    /// Returns the target inode number.
    pub const fn inode(self) -> InodeNumber {
        self.inode
    }

    /// Returns the ext4 directory file type.
    pub const fn file_type(self) -> DirectoryFileType {
        self.file_type
    }

    /// Returns this record's byte offset in the directory file.
    pub const fn offset(self) -> Ext4DirPos {
        self.offset
    }
}

/// Sink used by ext4 core directory streaming.
pub trait Ext4DirSink {
    /// Emits one directory entry.
    ///
    /// `next_pos` is the byte position for the next record. Returning `false`
    /// stops before consuming this entry without treating the directory as
    /// corrupt.
    fn emit(&mut self, entry: Ext4DirEntryRef<'_>, next_pos: Ext4DirPos) -> Ext4Result<bool>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ScannedDirectoryEntry<'a> {
    name: &'a [u8],
    inode: InodeNumber,
    file_type: DirectoryFileType,
    offset: u64,
    next_offset: u64,
}

impl Ext4SbInfo {
    /// Reads all supported directory entries from a directory inode.
    pub fn read_dir(&self, inode: &Ext4Inode) -> Ext4Result<Vec<DirectoryEntry>> {
        let mut entries = Vec::new();
        self.scan_directory_from(inode, Ext4DirPos::new(0), |entry| {
            entries.push(DirectoryEntry::from_scanned(entry));
            Ok(true)
        })?;
        Ok(entries)
    }

    /// Streams supported directory entries starting at an ext4 directory byte position.
    pub fn read_dir_from(
        &self,
        inode: &Ext4Inode,
        pos: Ext4DirPos,
        sink: &mut dyn Ext4DirSink,
    ) -> Ext4Result<Ext4DirPos> {
        let mut next_pos = pos;
        self.scan_directory_from(inode, pos, |entry| {
            let entry_ref = Ext4DirEntryRef {
                name: entry.name,
                inode: entry.inode,
                file_type: entry.file_type,
                offset: Ext4DirPos::new(entry.offset),
            };
            let emitted_next_pos = Ext4DirPos::new(entry.next_offset);
            if !sink.emit(entry_ref, emitted_next_pos)? {
                return Ok(false);
            }
            next_pos = emitted_next_pos;
            Ok(true)
        })?;
        Ok(next_pos)
    }

    /// Finds one UTF-8 entry name in a supported directory.
    pub fn lookup(&self, directory: &Ext4Inode, name: &str) -> Ext4Result<Option<DirectoryEntry>> {
        self.lookup_bytes(directory, name.as_bytes())
    }

    /// Finds one raw byte entry name in a supported directory.
    pub fn lookup_bytes(
        &self,
        directory: &Ext4Inode,
        name: &[u8],
    ) -> Ext4Result<Option<DirectoryEntry>> {
        let mut found = None;
        self.scan_directory_from(directory, Ext4DirPos::new(0), |entry| {
            if entry.name == name {
                found = Some(DirectoryEntry::from_scanned(entry));
                return Ok(false);
            }
            Ok(true)
        })?;
        Ok(found)
    }

    fn scan_directory_from<F>(
        &self,
        inode: &Ext4Inode,
        pos: Ext4DirPos,
        visitor: F,
    ) -> Ext4Result<()>
    where
        F: for<'a> FnMut(ScannedDirectoryEntry<'a>) -> Ext4Result<bool>,
    {
        if inode.kind() != InodeKind::Directory {
            return Err(Ext4Error::Unsupported(UnsupportedKind::InodeKind));
        }
        if pos.get() > inode.size() {
            return Err(Ext4Error::InvalidDirectoryPosition);
        }
        if pos.get() == inode.size() {
            return Ok(());
        }

        if inode.has_indexed_directory() {
            self.scan_indexed_directory_from(inode, pos, visitor)
        } else {
            self.scan_linear_directory_from(inode, pos, visitor)
        }
    }

    fn scan_linear_directory_from<F>(
        &self,
        inode: &Ext4Inode,
        pos: Ext4DirPos,
        mut visitor: F,
    ) -> Ext4Result<()>
    where
        F: for<'a> FnMut(ScannedDirectoryEntry<'a>) -> Ext4Result<bool>,
    {
        let block_size =
            usize::try_from(self.layout().block_size()).map_err(|_| Ext4Error::Overflow)?;
        let block_size_u64 = u64::try_from(block_size).map_err(|_| Ext4Error::Overflow)?;
        let mut block = vec![0; block_size];
        let mut block_start = (pos.get() / block_size_u64)
            .checked_mul(block_size_u64)
            .ok_or(Ext4Error::Overflow)?;
        while block_start < inode.size() {
            let logical_block = block_start / block_size_u64;
            let read_len =
                self.read_directory_block(inode, logical_block, block_size, &mut block)?;
            let block_bytes = block.get(..read_len).ok_or(Ext4Error::OutOfBounds)?;
            self.verify_directory_block(inode, logical_block, block_bytes)?;
            if !scan_directory_block(
                block_bytes,
                block_size,
                block_start,
                pos,
                self.superblock().inodes_count(),
                &mut visitor,
            )? {
                break;
            }
            block_start = block_start
                .checked_add(u64::try_from(read_len).map_err(|_| Ext4Error::Overflow)?)
                .ok_or(Ext4Error::Overflow)?;
        }
        Ok(())
    }

    fn scan_indexed_directory_from<F>(
        &self,
        inode: &Ext4Inode,
        pos: Ext4DirPos,
        mut visitor: F,
    ) -> Ext4Result<()>
    where
        F: for<'a> FnMut(ScannedDirectoryEntry<'a>) -> Ext4Result<bool>,
    {
        let block_size =
            usize::try_from(self.layout().block_size()).map_err(|_| Ext4Error::Overflow)?;
        let block_size_u64 = u64::try_from(block_size).map_err(|_| Ext4Error::Overflow)?;
        let block_count = directory_block_count(inode.size(), block_size_u64)?;
        if block_count == 0 {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }

        let mut root_block = vec![0; block_size];
        let root_len = self.read_directory_block(inode, 0, block_size, &mut root_block)?;
        if root_len != block_size {
            return Err(Ext4Error::Corrupt(CorruptKind::Truncated));
        }
        let root_bytes = root_block
            .get(..root_len)
            .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?;
        let leaf_blocks = self.decode_htree_root_leaf_blocks(inode, root_bytes, block_count)?;

        let pos_block = pos.get() / block_size_u64;
        if pos_block != 0
            && leaf_blocks
                .binary_search(&u32::try_from(pos_block).map_err(|_| Ext4Error::Overflow)?)
                .is_err()
        {
            return Err(Ext4Error::InvalidDirectoryPosition);
        }

        if !scan_directory_block(
            root_bytes,
            block_size,
            0,
            pos,
            self.superblock().inodes_count(),
            &mut visitor,
        )? {
            return Ok(());
        }

        let mut block = vec![0; block_size];
        for logical_block in leaf_blocks {
            let logical_block_u64 = u64::from(logical_block);
            let block_start = logical_block_u64
                .checked_mul(block_size_u64)
                .ok_or(Ext4Error::Overflow)?;
            let block_end = block_start
                .checked_add(block_size_u64)
                .ok_or(Ext4Error::Overflow)?;
            if block_end <= pos.get() {
                continue;
            }

            let read_len =
                self.read_directory_block(inode, logical_block_u64, block_size, &mut block)?;
            if read_len != block_size {
                return Err(Ext4Error::Corrupt(CorruptKind::Truncated));
            }
            let block_bytes = block.get(..read_len).ok_or(Ext4Error::OutOfBounds)?;
            self.verify_directory_block(inode, logical_block_u64, block_bytes)?;
            if !scan_directory_block(
                block_bytes,
                block_size,
                block_start,
                pos,
                self.superblock().inodes_count(),
                &mut visitor,
            )? {
                return Ok(());
            }
        }
        Ok(())
    }

    pub(crate) fn decode_htree_root_leaf_blocks(
        &self,
        inode: &Ext4Inode,
        bytes: &[u8],
        block_count: u64,
    ) -> Ext4Result<Vec<u32>> {
        let block_size =
            usize::try_from(self.layout().block_size()).map_err(|_| Ext4Error::Overflow)?;
        let dot = disk_dir::RawDirectoryEntry::decode(bytes, 0)?;
        let dot_rec_len = rec_len_from_disk(dot.rec_len(), block_size)?;
        if dot_rec_len != 12 {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }
        let dotdot = disk_dir::RawDirectoryEntry::decode(bytes, dot_rec_len)?;
        let dotdot_rec_len = rec_len_from_disk(dotdot.rec_len(), block_size)?;
        if dotdot_rec_len
            != block_size
                .checked_sub(dot_rec_len)
                .ok_or(Ext4Error::Overflow)?
        {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }

        let root_info = disk_dir::HTreeRootInfo::decode(bytes)?;
        if root_info.reserved_zero() != 0
            || root_info.info_length() != 8
            || root_info.flags() != 0
            || root_info.hash_version() > 6
        {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }
        if root_info.indirect_levels() > disk_dir::DX_MAX_TREE_DEPTH_WITHOUT_LARGEDIR {
            if self.superblock().features().has_large_dir()
                && root_info.indirect_levels() <= disk_dir::DX_MAX_TREE_DEPTH_WITH_LARGEDIR
            {
                return Err(Ext4Error::Unsupported(UnsupportedKind::LargeDir));
            }
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }

        let count_limit =
            self.decode_htree_count_limit(bytes, disk_dir::DX_ROOT_COUNT_LIMIT_OFFSET, block_size)?;
        self.verify_htree_block_checksum(
            inode,
            0,
            bytes,
            disk_dir::DX_ROOT_COUNT_LIMIT_OFFSET,
            count_limit,
        )?;

        let mut leaf_blocks = Vec::with_capacity(usize::from(count_limit.count()));
        self.collect_htree_leaf_blocks_from_entries(
            inode,
            bytes,
            disk_dir::DX_ROOT_COUNT_LIMIT_OFFSET,
            count_limit,
            root_info.indirect_levels(),
            block_count,
            &mut leaf_blocks,
        )?;
        leaf_blocks.sort_unstable();
        if leaf_blocks.windows(2).any(|window| window[0] == window[1]) {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }
        Ok(leaf_blocks)
    }

    #[allow(clippy::too_many_arguments)]
    fn collect_htree_leaf_blocks_from_entries(
        &self,
        inode: &Ext4Inode,
        bytes: &[u8],
        count_offset: usize,
        count_limit: disk_dir::HTreeCountLimit,
        indirect_levels: u8,
        block_count: u64,
        leaf_blocks: &mut Vec<u32>,
    ) -> Ext4Result<()> {
        let mut previous_hash = None;
        for index in 0..usize::from(count_limit.count()) {
            let entry = disk_dir::HTreeEntry::decode_indexed(bytes, count_offset, index)?;
            if previous_hash.is_some_and(|hash| entry.hash() < hash) {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
            }
            let block = entry.block();
            if block == 0 || u64::from(block) >= block_count {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
            }
            previous_hash = Some(entry.hash());
            if indirect_levels == 0 {
                leaf_blocks.push(block);
            } else {
                self.collect_htree_leaf_blocks_from_node(
                    inode,
                    u64::from(block),
                    indirect_levels - 1,
                    block_count,
                    leaf_blocks,
                )?;
            }
        }
        Ok(())
    }

    fn collect_htree_leaf_blocks_from_node(
        &self,
        inode: &Ext4Inode,
        logical_block: u64,
        indirect_levels: u8,
        block_count: u64,
        leaf_blocks: &mut Vec<u32>,
    ) -> Ext4Result<()> {
        let block_size =
            usize::try_from(self.layout().block_size()).map_err(|_| Ext4Error::Overflow)?;
        let mut node_block = vec![0; block_size];
        let read_len =
            self.read_directory_block(inode, logical_block, block_size, &mut node_block)?;
        if read_len != block_size {
            return Err(Ext4Error::Corrupt(CorruptKind::Truncated));
        }
        let bytes = node_block
            .get(..read_len)
            .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?;
        let fake_dirent = disk_dir::RawDirectoryEntry::decode(bytes, 0)?;
        let fake_rec_len = rec_len_from_disk(fake_dirent.rec_len(), block_size)?;
        if fake_dirent.inode() != 0 || fake_dirent.name_len() != 0 || fake_rec_len != block_size {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }

        let count_limit =
            self.decode_htree_count_limit(bytes, disk_dir::DX_NODE_COUNT_LIMIT_OFFSET, block_size)?;
        self.verify_htree_block_checksum(
            inode,
            logical_block,
            bytes,
            disk_dir::DX_NODE_COUNT_LIMIT_OFFSET,
            count_limit,
        )?;
        self.collect_htree_leaf_blocks_from_entries(
            inode,
            bytes,
            disk_dir::DX_NODE_COUNT_LIMIT_OFFSET,
            count_limit,
            indirect_levels,
            block_count,
            leaf_blocks,
        )
    }

    pub(crate) fn decode_htree_count_limit(
        &self,
        bytes: &[u8],
        count_offset: usize,
        block_size: usize,
    ) -> Ext4Result<disk_dir::HTreeCountLimit> {
        let count_limit = disk_dir::HTreeCountLimit::decode(bytes, count_offset)?;
        let limit = usize::from(count_limit.limit());
        let count = usize::from(count_limit.count());
        if limit == 0 || count == 0 || count > limit {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }

        let entries_end = count_offset
            .checked_add(
                limit
                    .checked_mul(disk_dir::DX_ENTRY_SIZE)
                    .ok_or(Ext4Error::Overflow)?,
            )
            .ok_or(Ext4Error::Overflow)?;
        let max_entries_end = if self.superblock().features().has_metadata_checksum() {
            block_size
                .checked_sub(disk_dir::DX_TAIL_SIZE)
                .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?
        } else {
            block_size
        };
        if entries_end > max_entries_end {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }
        Ok(count_limit)
    }

    fn read_directory_block(
        &self,
        inode: &Ext4Inode,
        logical_block: u64,
        block_size: usize,
        output: &mut [u8],
    ) -> Ext4Result<usize> {
        if output.len() < block_size {
            return Err(Ext4Error::InvalidBufferLength {
                expected: block_size,
                actual: output.len(),
            });
        }
        let block_size_u64 = u64::try_from(block_size).map_err(|_| Ext4Error::Overflow)?;
        let block_start = logical_block
            .checked_mul(block_size_u64)
            .ok_or(Ext4Error::Overflow)?;
        let remaining = inode
            .size()
            .checked_sub(block_start)
            .ok_or(Ext4Error::Overflow)?;
        let read_len = if remaining < block_size_u64 {
            usize::try_from(remaining).map_err(|_| Ext4Error::Overflow)?
        } else {
            block_size
        };

        match self.map_blocks(inode, LogicalBlock::new(logical_block))? {
            BlockMapping::Mapped { physical, len, .. } if len.get() != 0 => {
                let buffer = self.read_metadata_block(FilesystemBlock::new(physical.get()))?;
                output[..read_len].copy_from_slice(&buffer.as_ref()[..read_len]);
            }
            BlockMapping::Hole { .. } | BlockMapping::Unwritten { .. } => {
                output[..read_len].fill(0);
            }
            BlockMapping::Mapped { .. } => {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
            }
        }
        Ok(read_len)
    }

    pub(crate) fn verify_htree_block_checksum(
        &self,
        inode: &Ext4Inode,
        logical_block: u64,
        bytes: &[u8],
        count_offset: usize,
        count_limit: disk_dir::HTreeCountLimit,
    ) -> Ext4Result<()> {
        if !self.superblock().features().has_metadata_checksum() {
            return Ok(());
        }
        let limit = usize::from(count_limit.limit());
        let count = usize::from(count_limit.count());
        let tail_offset = count_offset
            .checked_add(
                limit
                    .checked_mul(disk_dir::DX_ENTRY_SIZE)
                    .ok_or(Ext4Error::Overflow)?,
            )
            .ok_or(Ext4Error::Overflow)?;
        let tail_end = tail_offset
            .checked_add(disk_dir::DX_TAIL_SIZE)
            .ok_or(Ext4Error::Overflow)?;
        if tail_end > bytes.len() {
            return Err(Ext4Error::Corrupt(CorruptKind::Truncated));
        }
        let used_len = count_offset
            .checked_add(
                count
                    .checked_mul(disk_dir::DX_ENTRY_SIZE)
                    .ok_or(Ext4Error::Overflow)?,
            )
            .ok_or(Ext4Error::Overflow)?;
        let used_bytes = bytes
            .get(..used_len)
            .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?;
        let tail_reserved = bytes
            .get(tail_offset..tail_offset + 4)
            .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?;

        let mut actual = checksum::crc32c(
            self.superblock().checksum_seed(),
            &inode.number().get().to_le_bytes(),
        );
        actual = checksum::crc32c(actual, &inode.generation().to_le_bytes());
        actual = checksum::crc32c(actual, used_bytes);
        actual = checksum::crc32c(actual, tail_reserved);
        actual = checksum::crc32c(actual, &0u32.to_le_bytes());
        let expected = codec::le_u32(bytes, tail_offset + 4)?;
        if actual != expected {
            return Err(Ext4Error::ChecksumMismatch {
                target: ChecksumTarget::DirectoryBlock {
                    inode: inode.number().get(),
                    block: logical_block,
                },
                expected,
                actual,
            });
        }
        Ok(())
    }

    pub(crate) fn verify_directory_block(
        &self,
        inode: &Ext4Inode,
        logical_block: u64,
        bytes: &[u8],
    ) -> Ext4Result<()> {
        if !self.superblock().features().has_metadata_checksum() {
            return Ok(());
        }
        if bytes.len()
            != usize::try_from(self.layout().block_size()).map_err(|_| Ext4Error::Overflow)?
        {
            return Ok(());
        }
        let tail_offset = bytes
            .len()
            .checked_sub(disk_dir::DIRENT_TAIL_SIZE)
            .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?;
        let tail = disk_dir::RawDirectoryEntry::decode(bytes, tail_offset)?;
        if !tail.is_checksum_tail() {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }
        let checksum_bytes = bytes
            .get(..tail_offset)
            .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?;
        let seed = inode_checksum_seed(
            self.superblock().checksum_seed(),
            inode.number(),
            inode.generation(),
        );
        let actual = checksum::crc32c(seed, checksum_bytes);
        let expected = disk_dir::tail_checksum(bytes)?;
        if actual != expected {
            return Err(Ext4Error::ChecksumMismatch {
                target: ChecksumTarget::DirectoryBlock {
                    inode: inode.number().get(),
                    block: logical_block,
                },
                expected,
                actual,
            });
        }
        Ok(())
    }
}

fn directory_block_count(size: u64, block_size: u64) -> Ext4Result<u64> {
    size.checked_add(block_size.checked_sub(1).ok_or(Ext4Error::Overflow)?)
        .ok_or(Ext4Error::Overflow)?
        .checked_div(block_size)
        .ok_or(Ext4Error::Overflow)
}

fn scan_directory_block(
    bytes: &[u8],
    block_size: usize,
    base_offset: u64,
    start_pos: Ext4DirPos,
    inodes_count: u32,
    visitor: &mut impl for<'a> FnMut(ScannedDirectoryEntry<'a>) -> Ext4Result<bool>,
) -> Ext4Result<bool> {
    let mut offset = 0usize;
    while offset < bytes.len() {
        let entry = disk_dir::RawDirectoryEntry::decode(bytes, offset)?;
        let rec_len = rec_len_from_disk(entry.rec_len(), block_size)?;
        if rec_len == 0 || rec_len % 4 != 0 || rec_len < disk_dir::DIRENT_HEADER_SIZE {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }
        let next = offset.checked_add(rec_len).ok_or(Ext4Error::Overflow)?;
        if next > bytes.len() {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }
        let entry_offset = base_offset
            .checked_add(u64::try_from(offset).map_err(|_| Ext4Error::Overflow)?)
            .ok_or(Ext4Error::Overflow)?;
        let next_offset = base_offset
            .checked_add(u64::try_from(next).map_err(|_| Ext4Error::Overflow)?)
            .ok_or(Ext4Error::Overflow)?;
        if next_offset <= start_pos.get() {
            offset = next;
            continue;
        }
        if entry_offset < start_pos.get() {
            return Err(Ext4Error::InvalidDirectoryPosition);
        }
        if entry.is_checksum_tail() {
            if next != bytes.len() {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
            }
            break;
        }
        let name_len = usize::from(entry.name_len());
        if name_len > disk_dir::DIRENT_NAME_MAX
            || disk_dir::DIRENT_HEADER_SIZE
                .checked_add(name_len)
                .ok_or(Ext4Error::Overflow)?
                > rec_len
        {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
        }
        if entry.inode() != 0 {
            if entry.inode() > inodes_count {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidDirectoryEntry));
            }
            let name_start = offset
                .checked_add(disk_dir::DIRENT_HEADER_SIZE)
                .ok_or(Ext4Error::Overflow)?;
            let name_end = name_start
                .checked_add(name_len)
                .ok_or(Ext4Error::Overflow)?;
            let name = bytes
                .get(name_start..name_end)
                .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?;
            let scanned = ScannedDirectoryEntry {
                name,
                inode: InodeNumber::new(entry.inode()),
                file_type: entry.file_type(),
                offset: entry_offset,
                next_offset,
            };
            if !visitor(scanned)? {
                return Ok(false);
            }
        }
        offset = next;
    }
    Ok(true)
}

pub(crate) fn rec_len_from_disk(raw: u16, block_size: usize) -> Ext4Result<usize> {
    let len = usize::from(raw);
    if len == u16::MAX as usize || len == 0 {
        return Ok(block_size);
    }
    let high_bits = len.checked_shl(16).ok_or(Ext4Error::Overflow)? & 0x30000;
    Ok((len & 0xfffc) | high_bits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_dirent_file_type_scans_as_unknown() {
        let mut bytes = [0; 12];
        put_u32(&mut bytes, 0x00, 2);
        put_u16(&mut bytes, 0x04, 12);
        bytes[0x06] = 1;
        bytes[0x07] = 99;
        bytes[0x08] = b"x"[0];

        let mut seen = None;
        let completed =
            scan_directory_block(&bytes, 4_096, 0, Ext4DirPos::new(0), 10, &mut |entry| {
                seen = Some((Vec::from(entry.name), entry.file_type));
                Ok(true)
            })
            .expect("scan directory block");

        assert!(completed);
        assert_eq!(
            seen,
            Some((Vec::from(&b"x"[..]), DirectoryFileType::Unknown))
        );
    }

    #[test]
    fn non_boundary_start_position_is_invalid() {
        let mut bytes = [0; 12];
        put_u32(&mut bytes, 0x00, 2);
        put_u16(&mut bytes, 0x04, 12);
        bytes[0x06] = 1;
        bytes[0x07] = 1;
        bytes[0x08] = b'x';

        let mut emitted = false;
        let error = scan_directory_block(&bytes, 4_096, 0, Ext4DirPos::new(1), 10, &mut |_| {
            emitted = true;
            Ok(true)
        })
        .expect_err("reject non-boundary directory position");

        assert_eq!(error, Ext4Error::InvalidDirectoryPosition);
        assert!(!emitted);
    }

    fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
        bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
}

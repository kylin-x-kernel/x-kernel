// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{vec, vec::Vec};

#[cfg(any(test, unittest))]
use crate::PhysicalBlock;
use crate::{
    BlockCount, CorruptKind, Ext4Error, Ext4Result, Ext4SbInfo, FilesystemBlock, LogicalBlock,
    UnsupportedKind,
    extent::{self, BlockMapping, ExtentMappingState},
    inode::{Ext4Inode, Ext4Timestamp, InodeKind},
    jbd2::JournalCredits,
    mballoc::{Ext4AllocationFlags, Ext4AllocationRequest},
};

#[cfg(test)]
mod raw_overwrite_tests;

struct AllocatedWriteRun {
    first_block: FilesystemBlock,
    block_count: u32,
    input_offset: usize,
    in_block: usize,
    write_len: usize,
}

#[derive(Clone, Copy, Default)]
struct OrderedWritebackCreditUsage {
    allocation_runs: u32,
    extent_mutations: u32,
}

impl OrderedWritebackCreditUsage {
    fn credit_bound(self) -> Ext4Result<u32> {
        extent::ordered_writeback_credit_bound(self.allocation_runs, self.extent_mutations)
    }

    fn with_unwritten_mapping(self) -> Ext4Result<Self> {
        Ok(Self {
            extent_mutations: self
                .extent_mutations
                .checked_add(1)
                .ok_or(Ext4Error::Overflow)?,
            ..self
        })
    }

    fn with_hole_allocation_run(self) -> Ext4Result<Self> {
        Ok(Self {
            allocation_runs: self
                .allocation_runs
                .checked_add(1)
                .ok_or(Ext4Error::Overflow)?,
            extent_mutations: self
                .extent_mutations
                .checked_add(2)
                .ok_or(Ext4Error::Overflow)?,
        })
    }

    fn with_mapping_operation(self, mapping: BlockMapping) -> Ext4Result<Self> {
        match mapping {
            BlockMapping::Mapped { .. } => Ok(self),
            BlockMapping::Unwritten { .. } => self.with_unwritten_mapping(),
            BlockMapping::Hole { .. } => self.with_hole_allocation_run(),
        }
    }
}

struct OrderedWritebackCursor {
    input_offset: usize,
    settled_input_offset: usize,
    remaining_extra_prealloc_blocks: u32,
}

impl OrderedWritebackCursor {
    const fn new(remaining_extra_prealloc_blocks: u32) -> Self {
        Self {
            input_offset: 0,
            settled_input_offset: 0,
            remaining_extra_prealloc_blocks,
        }
    }

    fn absolute_offset(&self, write_offset: u64) -> Ext4Result<u64> {
        write_offset
            .checked_add(u64::try_from(self.input_offset).map_err(|_| Ext4Error::Overflow)?)
            .ok_or(Ext4Error::Overflow)
    }

    fn advance(&mut self, byte_len: usize) -> Ext4Result<()> {
        self.input_offset = self
            .input_offset
            .checked_add(byte_len)
            .ok_or(Ext4Error::Overflow)?;
        Ok(())
    }

    const fn is_complete(&self, input_len: usize) -> bool {
        self.input_offset == input_len
    }

    const fn settle(&mut self) {
        self.settled_input_offset = self.input_offset;
    }

    const fn settled_failure(&self, error: Ext4Error) -> Ext4WritebackError {
        Ext4WritebackError::new(self.settled_input_offset, error)
    }

    const fn current_failure(&self, error: Ext4Error) -> Ext4WritebackError {
        Ext4WritebackError::new(self.input_offset, error)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OrderedWritebackTransactionProgress {
    Complete,
    Restart,
}

#[derive(Clone, Copy)]
enum PartialBlockWrite {
    PreserveExisting,
    ZeroFill,
}

const EXT4_PREALLOC_MIN_WRITE_BLOCKS: u32 = 4;
const EXT4_STREAM_PREALLOC_MULTIPLIER: u32 = 8;
const EXT4_RANDOM_PREALLOC_BLOCKS: u32 = 8;
const EXT4_MAX_PREALLOC_BLOCKS: u32 = 1024;

impl JournalCredits {
    const fn for_regular_inode_write_metadata() -> Self {
        Self::new(1)
    }
}

/// Sync intent for ordered-data writeback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Ext4SyncIntent {
    /// Flush data and commit only metadata required to make newly written data reachable.
    DataOnly,
    /// Commit the full regular-file metadata state visible through the VFS inode.
    FullMetadata,
}

/// Ordered-writeback failure together with its completed byte prefix.
///
/// The prefix is measured from the beginning of the submitted input. Bytes in
/// that prefix have completed the backing-filesystem transaction boundary and
/// may be ended as successful PageCache writeback; the remaining suffix must
/// stay dirty for retry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Ext4WritebackError {
    completed_bytes: usize,
    error: Ext4Error,
}

impl Ext4WritebackError {
    const fn new(completed_bytes: usize, error: Ext4Error) -> Self {
        Self {
            completed_bytes,
            error,
        }
    }

    /// Returns the completed byte prefix.
    pub const fn completed_bytes(self) -> usize {
        self.completed_bytes
    }

    /// Returns the error following the completed prefix.
    pub const fn error(self) -> Ext4Error {
        self.error
    }
}

impl From<Ext4Error> for Ext4WritebackError {
    fn from(error: Ext4Error) -> Self {
        Self::new(0, error)
    }
}

/// Result of one ordered writeback submission.
///
/// Success returns the complete input length. Failure carries both the source
/// error and the prefix completed by earlier committed transactions.
pub type Ext4WritebackResult = core::result::Result<usize, Ext4WritebackError>;

impl Ext4SyncIntent {
    /// Converts the KVFS `data_only` sync flag at the adapter boundary.
    pub const fn from_data_only(data_only: bool) -> Self {
        if data_only {
            Self::DataOnly
        } else {
            Self::FullMetadata
        }
    }

    /// Returns whether this sync is data-only.
    pub const fn is_data_only(self) -> bool {
        matches!(self, Self::DataOnly)
    }

    /// Returns whether this sync must commit all regular-file metadata.
    pub const fn requires_full_metadata(self) -> bool {
        matches!(self, Self::FullMetadata)
    }

    const fn write_metadata(self, timestamp: Ext4Timestamp) -> RegularWriteMetadata {
        match self {
            Self::DataOnly => RegularWriteMetadata::SizeOnly,
            Self::FullMetadata => RegularWriteMetadata::Full { timestamp },
        }
    }
}

/// Regular-file metadata to commit after ordered data has reached storage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RegularWriteMetadata {
    /// Commit only the size needed to make flushed data reachable.
    SizeOnly,
    /// Commit size and timestamp metadata.
    Full {
        /// Timestamp applied to ctime and mtime.
        timestamp: Ext4Timestamp,
    },
}

impl Ext4SbInfo {
    pub(crate) fn ensure_regular_file_mutation_supported(
        &self,
        inode: &Ext4Inode,
    ) -> Ext4Result<()> {
        if inode.kind() != InodeKind::RegularFile {
            return Err(Ext4Error::Unsupported(UnsupportedKind::InodeKind));
        }
        if !inode.has_extents() {
            return Err(Ext4Error::Unsupported(UnsupportedKind::NonExtentInode));
        }
        if inode.uses_huge_file_accounting() {
            if !self.superblock().features().has_huge_file() {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidInode));
            }
            return Err(Ext4Error::Unsupported(UnsupportedKind::HugeFile));
        }
        Ok(())
    }

    /// Commits metadata needed by this inode without forcing a checkpoint.
    ///
    /// KExt4 currently commits the whole running transaction because sync tids
    /// belong to the runtime inode rather than the mount-wide journal. The VFS
    /// inode will own targeted cursors once mutation completion reports tids;
    /// committing the shared transaction is conservative and correct meanwhile.
    pub fn sync_inode(&mut self, _inode: &Ext4Inode, _intent: Ext4SyncIntent) -> Ext4Result<()> {
        let result = match self.commit_running_metadata_transaction() {
            Ok(true) => Ok(()),
            Ok(false) => {
                // A mapped overwrite may dirty only file data and therefore have no
                // metadata transaction whose commit barrier can carry durability.
                self.flush_device()
            }
            Err(error) => Err(error),
        };
        result.map_err(|error| self.fail_journal_operation(error))
    }

    /// Reads bytes from a regular file inode.
    pub fn read_at(&self, inode: &Ext4Inode, offset: u64, output: &mut [u8]) -> Ext4Result<usize> {
        if inode.kind() != InodeKind::RegularFile {
            return Err(Ext4Error::Unsupported(UnsupportedKind::InodeKind));
        }
        self.read_inode_bytes(inode, offset, output)
    }

    /// Reads bytes from a symbolic link inode.
    pub fn read_link_at(
        &self,
        inode: &Ext4Inode,
        offset: u64,
        output: &mut [u8],
    ) -> Ext4Result<usize> {
        match self.fast_symlink_target(inode)? {
            Some(target) => self.read_fast_symlink_target(&target, offset, output),
            None => self.read_block_symlink_target(inode, offset, output),
        }
    }

    fn read_fast_symlink_target(
        &self,
        target: &[u8],
        offset: u64,
        output: &mut [u8],
    ) -> Ext4Result<usize> {
        let target_len = u64::try_from(target.len()).map_err(|_| Ext4Error::Overflow)?;
        if output.is_empty() || offset >= target_len {
            return Ok(0);
        }
        let start = usize::try_from(offset).map_err(|_| Ext4Error::Overflow)?;
        let remaining = target.len().checked_sub(start).ok_or(Ext4Error::Overflow)?;
        let copied = output.len().min(remaining);
        let end = start.checked_add(copied).ok_or(Ext4Error::Overflow)?;
        output[..copied].copy_from_slice(
            target
                .get(start..end)
                .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidInode))?,
        );
        Ok(copied)
    }

    pub(crate) fn read_inode_bytes(
        &self,
        inode: &Ext4Inode,
        offset: u64,
        output: &mut [u8],
    ) -> Ext4Result<usize> {
        if output.is_empty() || offset >= inode.size() {
            return Ok(0);
        }

        let remaining_file = inode
            .size()
            .checked_sub(offset)
            .ok_or(Ext4Error::Overflow)?;
        let remaining_output = match usize::try_from(remaining_file) {
            Ok(value) => value,
            Err(_) => output.len(),
        };
        let total = output.len().min(remaining_output);
        let block_size = u64::from(self.layout().block_size());
        let block_size_usize = usize::try_from(block_size).map_err(|_| Ext4Error::Overflow)?;
        let mut copied = 0usize;

        while copied < total {
            let absolute = offset
                .checked_add(u64::try_from(copied).map_err(|_| Ext4Error::Overflow)?)
                .ok_or(Ext4Error::Overflow)?;
            let logical = absolute / block_size;
            let in_block =
                usize::try_from(absolute % block_size).map_err(|_| Ext4Error::Overflow)?;
            let block_remaining = block_size_usize
                .checked_sub(in_block)
                .ok_or(Ext4Error::Overflow)?;
            let wanted = (total - copied).min(block_remaining);

            match self.map_blocks(inode, LogicalBlock::new(logical))? {
                BlockMapping::Hole { .. } | BlockMapping::Unwritten { .. } => {
                    // Sparse regular-file holes read as zeroes.
                    output[copied..copied + wanted].fill(0);
                    copied += wanted;
                }
                BlockMapping::Mapped { physical, len, .. } => {
                    if physical.get() == 0 || len.get() == 0 {
                        return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
                    }
                    let max_blocks = usize::try_from(len.get()).map_err(|_| Ext4Error::Overflow)?;
                    let max_bytes = max_blocks
                        .checked_mul(block_size_usize)
                        .and_then(|bytes| bytes.checked_sub(in_block))
                        .ok_or(Ext4Error::Overflow)?;
                    let read_len = (total - copied).min(max_bytes);
                    let first_block = FilesystemBlock::new(physical.get());
                    let covered_bytes =
                        read_len.checked_add(in_block).ok_or(Ext4Error::Overflow)?;
                    let rounded_bytes = covered_bytes
                        .checked_add(block_size_usize.checked_sub(1).ok_or(Ext4Error::Overflow)?)
                        .ok_or(Ext4Error::Overflow)?;
                    let block_count = rounded_bytes
                        .checked_div(block_size_usize)
                        .ok_or(Ext4Error::Overflow)?;
                    let mut block_bytes = vec![
                        0;
                        block_count
                            .checked_mul(block_size_usize)
                            .ok_or(Ext4Error::Overflow)?
                    ];
                    let block_count_u32 =
                        u32::try_from(block_count).map_err(|_| Ext4Error::Overflow)?;
                    self.read_blocks(first_block, block_count_u32, &mut block_bytes)?;
                    let source_end = in_block.checked_add(read_len).ok_or(Ext4Error::Overflow)?;
                    output[copied..copied + read_len].copy_from_slice(
                        block_bytes
                            .get(in_block..source_end)
                            .ok_or(Ext4Error::OutOfBounds)?,
                    );
                    copied += read_len;
                }
            }
        }

        Ok(copied)
    }

    /// Reads a block-mapped symlink target.
    ///
    /// The target is stored in logical block 0, a journaled metadata buffer
    /// written via `metadata_io.create_access`. It is read from the metadata
    /// cache so uncheckpointed content is visible, matching Linux
    /// `ext4_get_link()` which maps logical block 0 with `ext4_bread` and
    /// reads it through the buffer cache.
    fn read_block_symlink_target(
        &self,
        inode: &Ext4Inode,
        offset: u64,
        output: &mut [u8],
    ) -> Ext4Result<usize> {
        if output.is_empty() || offset >= inode.size() {
            return Ok(0);
        }
        let start = usize::try_from(offset).map_err(|_| Ext4Error::Overflow)?;
        match self.map_blocks(inode, LogicalBlock::new(0))? {
            BlockMapping::Hole { .. } | BlockMapping::Unwritten { .. } => {
                Err(Ext4Error::Corrupt(CorruptKind::InvalidInode))
            }
            BlockMapping::Mapped { physical, len, .. } => {
                if physical.get() == 0 || len.get() == 0 {
                    return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
                }
                let buffer = self
                    .metadata_io
                    .read_block(FilesystemBlock::new(physical.get()))?;
                // Truncate to the remaining target length so callers passing
                // oversized buffers (e.g. a full page via `read_folio`) never
                // observe block padding after the target, matching the fast
                // symlink and regular-file read paths. `read_link_at` already
                // guarantees `offset < inode.size()`; checked arithmetic keeps
                // this robust against corrupt inode sizes anyway.
                let remaining_target = usize::try_from(
                    inode
                        .size()
                        .checked_sub(offset)
                        .ok_or(Ext4Error::Overflow)?,
                )
                .map_err(|_| Ext4Error::Overflow)?;
                // A block symlink target fits in one block, so the offset is
                // inside it; defend against corrupt inode sizes anyway.
                let available = buffer
                    .as_ref()
                    .len()
                    .checked_sub(start)
                    .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidInode))?
                    .min(remaining_target);
                let copied = output.len().min(available);
                let end = start.checked_add(copied).ok_or(Ext4Error::Overflow)?;
                output[..copied].copy_from_slice(
                    buffer
                        .as_ref()
                        .get(start..end)
                        .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidInode))?,
                );
                Ok(copied)
            }
        }
    }

    /// Raw overwrite of bytes inside already allocated regular-file blocks.
    ///
    /// This is not the Linux ext4 write path. It bypasses VFS/page-cache
    /// coherence and JBD2 transaction handles, must not be wired to
    /// `FileNodeOps::write_at`, and deliberately does not allocate blocks,
    /// convert unwritten extents, extend `i_size`, or update inode timestamps.
    ///
    /// The full target byte range is validated before any data block is
    /// written. If the range reaches a hole or unwritten extent, this method
    /// returns `UnallocatedWrite` without modifying storage.
    #[cfg(test)]
    pub(crate) fn raw_overwrite_allocated_data_unjournaled(
        &self,
        inode: &Ext4Inode,
        offset: u64,
        input: &[u8],
    ) -> Ext4Result<usize> {
        if inode.kind() != InodeKind::RegularFile {
            return Err(Ext4Error::Unsupported(UnsupportedKind::InodeKind));
        }
        if input.is_empty() || offset >= inode.size() {
            return Ok(0);
        }

        let remaining_file = inode
            .size()
            .checked_sub(offset)
            .ok_or(Ext4Error::Overflow)?;
        let remaining_input = match usize::try_from(remaining_file) {
            Ok(value) => value,
            Err(_) => input.len(),
        };
        let total = input.len().min(remaining_input);
        let block_size = u64::from(self.layout().block_size());
        let block_size_usize = usize::try_from(block_size).map_err(|_| Ext4Error::Overflow)?;
        let runs = self.collect_allocated_write_runs(inode, offset, total, block_size_usize)?;

        let mut written = 0usize;
        for run in &runs {
            self.write_allocated_run(
                run,
                input,
                block_size_usize,
                PartialBlockWrite::PreserveExisting,
            )?;
            written = written
                .checked_add(run.write_len)
                .ok_or(Ext4Error::Overflow)?;
        }

        self.flush_device()?;
        Ok(written)
    }

    /// Writes a dirty page-cache range through the ordered-data path.
    ///
    /// A request may span multiple journal transactions. If a later
    /// transaction fails, [`Ext4WritebackError::completed_bytes`] identifies
    /// the earlier completed prefix so callers only retain or redirty the
    /// remaining suffix.
    /// Device or journal failures may abort the mounted journal, in which case
    /// recovery is required before the data can be submitted again.
    pub fn writeback_ordered_at(
        &mut self,
        inode: &Ext4Inode,
        offset: u64,
        input: &[u8],
        visible_size: u64,
        timestamp: Ext4Timestamp,
        intent: Ext4SyncIntent,
    ) -> Ext4WritebackResult {
        self.writeback_ordered_at_with_prealloc_budget(
            inode,
            offset,
            input,
            visible_size,
            timestamp,
            intent,
            u32::MAX,
        )
    }

    /// Writes a dirty page-cache range through the ordered-data path with a
    /// cap on optional preallocation beyond the dirty range itself.
    ///
    /// A request may span multiple journal transactions. If a later
    /// transaction fails, [`Ext4WritebackError::completed_bytes`] identifies
    /// the earlier completed prefix so callers only retain or redirty the
    /// remaining suffix.
    /// Device or journal failures may abort the mounted journal, in which case
    /// recovery is required before the data can be submitted again.
    #[allow(clippy::too_many_arguments)]
    pub fn writeback_ordered_at_with_prealloc_budget(
        &mut self,
        inode: &Ext4Inode,
        offset: u64,
        input: &[u8],
        visible_size: u64,
        timestamp: Ext4Timestamp,
        intent: Ext4SyncIntent,
        max_extra_prealloc_blocks: u32,
    ) -> Ext4WritebackResult {
        self.ensure_regular_file_mutation_supported(inode)?;
        if self.journal.is_none() && intent.requires_full_metadata() {
            return Err(Ext4Error::Unsupported(UnsupportedKind::JournaledWrite).into());
        }
        let write_end = offset
            .checked_add(u64::try_from(input.len()).map_err(|_| Ext4Error::Overflow)?)
            .ok_or(Ext4Error::Overflow)?;
        if write_end > visible_size || visible_size < inode.disk_size() {
            return Err(Ext4Error::Unsupported(UnsupportedKind::UnallocatedWrite).into());
        }
        if intent.requires_full_metadata() {
            self.validate_inode_timestamp_update(inode, timestamp)?;
        }

        if input.is_empty() {
            if intent.is_data_only() && visible_size == inode.disk_size() {
                return Ok(0);
            }
            return self
                .commit_regular_inode_write_metadata(
                    inode,
                    visible_size,
                    intent.write_metadata(timestamp),
                )
                .map(|()| 0)
                .map_err(Into::into);
        }

        let block_size_usize =
            usize::try_from(self.layout().block_size()).map_err(|_| Ext4Error::Overflow)?;
        let needs_metadata = intent.requires_full_metadata()
            || write_end > inode.disk_size()
            || self.ordered_writeback_range_needs_metadata(
                inode,
                offset,
                input.len(),
                block_size_usize,
            )?;

        if !needs_metadata {
            let runs =
                self.collect_allocated_write_runs(inode, offset, input.len(), block_size_usize)?;
            let mut completed_bytes = 0usize;
            for run in &runs {
                if let Err(error) = self.write_allocated_run(
                    run,
                    input,
                    block_size_usize,
                    PartialBlockWrite::PreserveExisting,
                ) {
                    return Err(Ext4WritebackError::new(completed_bytes, error));
                }
                completed_bytes = completed_bytes
                    .checked_add(run.write_len)
                    .ok_or(Ext4Error::Overflow)?;
            }
            let mut cursor = OrderedWritebackCursor::new(0);
            cursor.advance(input.len())?;
            if let Err(error) = self.release_ordered_writeback_cursor_range(
                inode,
                offset,
                block_size_usize,
                &cursor,
            ) {
                return Err(cursor.current_failure(error));
            }
            return Ok(input.len());
        }

        // Required dirty data has already been reserved by write_begin or
        // page_mkwrite. Optional preallocation is limited to currently
        // unreserved space; execution re-reads the live mapping before every
        // mutation and reports any later ENOSPC with its completed prefix.
        let extra_prealloc_blocks = u32::try_from(
            self.unreserved_blocks_count()
                .min(u64::from(max_extra_prealloc_blocks)),
        )
        .map_err(|_| Ext4Error::Overflow)?;
        let mut cursor = OrderedWritebackCursor::new(extra_prealloc_blocks);
        loop {
            let absolute = cursor
                .absolute_offset(offset)
                .map_err(|error| cursor.settled_failure(error))?;
            let logical = LogicalBlock::new(
                absolute
                    / u64::try_from(block_size_usize)
                        .map_err(|_| cursor.settled_failure(Ext4Error::Overflow))?,
            );
            let first_mapping = self
                .map_blocks(inode, logical)
                .map_err(|error| cursor.settled_failure(error))?;
            validate_ordered_writeback_mapping(first_mapping)
                .map_err(|error| cursor.settled_failure(error))?;
            let usage = OrderedWritebackCreditUsage::default()
                .with_mapping_operation(first_mapping)
                .map_err(|error| cursor.settled_failure(error))?;
            let credits = JournalCredits::new(
                self.extent_writeback_metadata_credits(
                    inode,
                    usage.allocation_runs,
                    usage.extent_mutations,
                )
                .map_err(|error| cursor.settled_failure(error))?,
            );
            let journal = self
                .metadata_journal_for_mutation(
                    credits,
                    crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
                )
                .map_err(|error| cursor.settled_failure(error))?;
            let mut handle = journal
                .begin(credits)
                .map_err(|error| cursor.settled_failure(error))?;
            let result = self.writeback_ordered_metadata_transaction(
                inode,
                offset,
                input,
                visible_size,
                timestamp,
                intent,
                block_size_usize,
                first_mapping,
                usage,
                &mut cursor,
                &mut handle,
            );
            let progress = match result {
                Ok(progress) => progress,
                Err(error) => {
                    let error = self
                        .complete_metadata_mutation::<()>(handle, Err(error))
                        .expect_err("ordered writeback transaction returned an error");
                    return Err(cursor.settled_failure(error));
                }
            };
            match progress {
                OrderedWritebackTransactionProgress::Complete => {
                    if let Err(error) = self.complete_metadata_mutation(handle, Ok(())) {
                        return Err(cursor.settled_failure(error));
                    }
                    if let Err(error) = self.release_ordered_writeback_cursor_range(
                        inode,
                        offset,
                        block_size_usize,
                        &cursor,
                    ) {
                        return Err(cursor.current_failure(error));
                    }
                    return Ok(input.len());
                }
                OrderedWritebackTransactionProgress::Restart => {
                    if let Err(error) = self.complete_metadata_mutation_and_commit_with_policy(
                        handle,
                        Ok(()),
                        crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
                    ) {
                        return Err(cursor.settled_failure(error));
                    }
                    if let Err(error) = self.release_ordered_writeback_cursor_range(
                        inode,
                        offset,
                        block_size_usize,
                        &cursor,
                    ) {
                        return Err(cursor.current_failure(error));
                    }
                    cursor.settle();
                }
            }
        }
    }

    fn ordered_writeback_range_needs_metadata(
        &self,
        inode: &Ext4Inode,
        offset: u64,
        total: usize,
        block_size_usize: usize,
    ) -> Ext4Result<bool> {
        let block_size = u64::try_from(block_size_usize).map_err(|_| Ext4Error::Overflow)?;
        let write_end = offset
            .checked_add(u64::try_from(total).map_err(|_| Ext4Error::Overflow)?)
            .ok_or(Ext4Error::Overflow)?;
        let logical_end = write_end
            .checked_add(block_size.checked_sub(1).ok_or(Ext4Error::Overflow)?)
            .ok_or(Ext4Error::Overflow)?
            / block_size;
        let mut logical = offset / block_size;
        while logical < logical_end {
            let mapping = self.map_blocks(inode, LogicalBlock::new(logical))?;
            let mapping_len = validate_ordered_writeback_mapping(mapping)?;
            if !matches!(mapping, BlockMapping::Mapped { .. }) {
                return Ok(true);
            }
            logical = logical
                .checked_add(u64::from(mapping_len))
                .ok_or(Ext4Error::Overflow)?;
        }
        Ok(false)
    }

    fn release_ordered_writeback_cursor_range(
        &mut self,
        inode: &Ext4Inode,
        write_offset: u64,
        block_size_usize: usize,
        cursor: &OrderedWritebackCursor,
    ) -> Ext4Result<()> {
        if cursor.input_offset == cursor.settled_input_offset {
            return Ok(());
        }
        let block_size = u64::try_from(block_size_usize).map_err(|_| Ext4Error::Overflow)?;
        let settled_offset = write_offset
            .checked_add(
                u64::try_from(cursor.settled_input_offset).map_err(|_| Ext4Error::Overflow)?,
            )
            .ok_or(Ext4Error::Overflow)?;
        let completed_offset = write_offset
            .checked_add(u64::try_from(cursor.input_offset).map_err(|_| Ext4Error::Overflow)?)
            .ok_or(Ext4Error::Overflow)?;
        let first = settled_offset / block_size;
        let end = completed_offset
            .checked_add(block_size.checked_sub(1).ok_or(Ext4Error::Overflow)?)
            .ok_or(Ext4Error::Overflow)?
            / block_size;
        self.release_delalloc_range(
            inode,
            LogicalBlock::new(first),
            end.checked_sub(first).ok_or(Ext4Error::Overflow)?,
        )
    }

    /// Commits regular-file size and timestamps after ordered data writeback.
    pub(crate) fn commit_regular_inode_write_metadata(
        &mut self,
        inode: &Ext4Inode,
        disk_size: u64,
        metadata: RegularWriteMetadata,
    ) -> Ext4Result<()> {
        self.ensure_regular_file_mutation_supported(inode)?;
        if disk_size < inode.disk_size() {
            return Err(Ext4Error::Unsupported(UnsupportedKind::FileSizeShrink));
        }
        if let RegularWriteMetadata::Full { timestamp } = metadata {
            self.validate_inode_timestamp_update(inode, timestamp)?;
        }
        let credits = JournalCredits::for_regular_inode_write_metadata();
        let journal = self.metadata_journal_for_mutation(
            credits,
            crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
        )?;
        let mut handle = journal.begin(credits)?;
        let result =
            self.update_regular_inode_write_metadata(inode, disk_size, metadata, &mut handle);
        self.complete_metadata_mutation(handle, result)
    }

    #[allow(clippy::too_many_arguments)]
    fn writeback_ordered_metadata_transaction(
        &mut self,
        inode: &Ext4Inode,
        offset: u64,
        input: &[u8],
        visible_size: u64,
        timestamp: Ext4Timestamp,
        intent: Ext4SyncIntent,
        block_size_usize: usize,
        mut mapping: BlockMapping,
        mut usage: OrderedWritebackCreditUsage,
        cursor: &mut OrderedWritebackCursor,
        handle: &mut crate::jbd2::JournalHandle<'_>,
    ) -> Ext4Result<OrderedWritebackTransactionProgress> {
        let block_size = u64::try_from(block_size_usize).map_err(|_| Ext4Error::Overflow)?;

        loop {
            let absolute = cursor.absolute_offset(offset)?;
            let logical = LogicalBlock::new(absolute / block_size);
            let in_block =
                usize::try_from(absolute % block_size).map_err(|_| Ext4Error::Overflow)?;
            match mapping {
                BlockMapping::Mapped { physical, len, .. } => {
                    let write_len = mapping_write_len(
                        input
                            .len()
                            .checked_sub(cursor.input_offset)
                            .ok_or(Ext4Error::OutOfBounds)?,
                        len.get(),
                        in_block,
                        block_size_usize,
                    )?;
                    let run = AllocatedWriteRun {
                        first_block: FilesystemBlock::new(physical.get()),
                        block_count: block_count_for_byte_span(
                            in_block,
                            write_len,
                            block_size_usize,
                        )?,
                        input_offset: cursor.input_offset,
                        in_block,
                        write_len,
                    };
                    self.write_allocated_run(
                        &run,
                        input,
                        block_size_usize,
                        PartialBlockWrite::PreserveExisting,
                    )?;
                    cursor.advance(write_len)?;
                }
                BlockMapping::Unwritten { physical, len, .. } => {
                    let write_len = mapping_write_len(
                        input
                            .len()
                            .checked_sub(cursor.input_offset)
                            .ok_or(Ext4Error::OutOfBounds)?,
                        len.get(),
                        in_block,
                        block_size_usize,
                    )?;
                    let block_count =
                        block_count_for_byte_span(in_block, write_len, block_size_usize)?;
                    let run = AllocatedWriteRun {
                        first_block: FilesystemBlock::new(physical.get()),
                        block_count,
                        input_offset: cursor.input_offset,
                        in_block,
                        write_len,
                    };
                    self.write_allocated_run(
                        &run,
                        input,
                        block_size_usize,
                        PartialBlockWrite::ZeroFill,
                    )?;
                    self.convert_unwritten_extent_range(
                        inode,
                        logical,
                        BlockCount::new(block_count),
                        handle,
                    )?;
                    cursor.advance(write_len)?;
                }
                BlockMapping::Hole { len, .. } => {
                    let remaining_blocks = block_count_for_byte_span(
                        in_block,
                        input
                            .len()
                            .checked_sub(cursor.input_offset)
                            .ok_or(Ext4Error::OutOfBounds)?,
                        block_size_usize,
                    )?;
                    let requested = remaining_blocks.min(len.get());
                    if requested == 0 {
                        return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
                    }
                    let locality_group = self.block_group_for_inode(inode.number())?;
                    let goal = self.allocation_goal_after_previous_extent(inode, logical)?;
                    let normalized =
                        normalize_write_allocation_len(requested, len.get(), goal.is_some());
                    let extra = normalized
                        .checked_sub(requested)
                        .ok_or(Ext4Error::Overflow)?
                        .min(cursor.remaining_extra_prealloc_blocks);
                    let expected = requested.checked_add(extra).ok_or(Ext4Error::Overflow)?;
                    let request = Ext4AllocationRequest::new(
                        logical,
                        goal,
                        BlockCount::new(expected),
                        BlockCount::new(1),
                        Ext4AllocationFlags::ALLOW_PARTIAL,
                        locality_group,
                    )?;
                    let allocation = self.allocate_blocks_for_write(request, handle)?;
                    let allocated_blocks = allocation.block_count();
                    self.insert_extent_mapping(
                        inode,
                        logical,
                        allocation.physical_start(),
                        allocated_blocks,
                        ExtentMappingState::Unwritten,
                        handle,
                    )?;

                    let write_len = mapping_write_len(
                        input
                            .len()
                            .checked_sub(cursor.input_offset)
                            .ok_or(Ext4Error::OutOfBounds)?,
                        allocated_blocks.get(),
                        in_block,
                        block_size_usize,
                    )?;
                    let block_count =
                        block_count_for_byte_span(in_block, write_len, block_size_usize)?;
                    if allocated_blocks.get() > block_count {
                        let extra_blocks = allocated_blocks.get() - block_count;
                        cursor.remaining_extra_prealloc_blocks = cursor
                            .remaining_extra_prealloc_blocks
                            .saturating_sub(extra_blocks);
                    }
                    let run = AllocatedWriteRun {
                        first_block: FilesystemBlock::new(allocation.physical_start().get()),
                        block_count,
                        input_offset: cursor.input_offset,
                        in_block,
                        write_len,
                    };
                    self.write_allocated_run(
                        &run,
                        input,
                        block_size_usize,
                        PartialBlockWrite::ZeroFill,
                    )?;
                    self.convert_unwritten_extent_range(
                        inode,
                        logical,
                        BlockCount::new(block_count),
                        handle,
                    )?;
                    cursor.advance(write_len)?;
                }
            }

            if cursor.is_complete(input.len()) {
                self.flush_device()?;
                let write_end = cursor.absolute_offset(offset)?;
                let disk_size = if intent.is_data_only() {
                    inode.disk_size().max(write_end)
                } else {
                    visible_size
                };
                self.update_regular_inode_write_metadata(
                    inode,
                    disk_size,
                    intent.write_metadata(timestamp),
                    handle,
                )?;
                return Ok(OrderedWritebackTransactionProgress::Complete);
            }

            let next_absolute = cursor.absolute_offset(offset)?;
            let next_logical = LogicalBlock::new(next_absolute / block_size);
            let next_mapping = self.map_blocks(inode, next_logical)?;
            validate_ordered_writeback_mapping(next_mapping)?;
            let candidate_usage = usage.with_mapping_operation(next_mapping)?;
            let current_bound = usage.credit_bound()?;
            let candidate_bound = candidate_usage.credit_bound()?;
            let operation_credits = candidate_bound
                .checked_sub(current_bound)
                .ok_or(Ext4Error::Overflow)?;
            let final_inode_credits = OrderedWritebackCreditUsage::default().credit_bound()?;
            let required_remaining = final_inode_credits
                .checked_add(operation_credits)
                .ok_or(Ext4Error::Overflow)?;
            let additional_credits = required_remaining.saturating_sub(handle.remaining_credits());
            if additional_credits > 0 {
                match handle.reserve_more(JournalCredits::new(additional_credits)) {
                    Ok(()) => {}
                    Err(Ext4Error::InsufficientJournalCredits) => {
                        self.flush_device()?;
                        let durable_size = inode.disk_size().max(cursor.absolute_offset(offset)?);
                        self.update_regular_inode_write_metadata(
                            inode,
                            durable_size,
                            RegularWriteMetadata::SizeOnly,
                            handle,
                        )?;
                        return Ok(OrderedWritebackTransactionProgress::Restart);
                    }
                    Err(error) => return Err(error),
                }
            }
            usage = candidate_usage;
            mapping = next_mapping;
        }
    }

    pub(crate) fn allocation_goal_after_previous_extent(
        &self,
        inode: &Ext4Inode,
        logical: LogicalBlock,
    ) -> Ext4Result<Option<FilesystemBlock>> {
        let Some(previous_logical) = logical.get().checked_sub(1) else {
            return Ok(None);
        };
        match self.map_blocks(inode, LogicalBlock::new(previous_logical))? {
            BlockMapping::Mapped { physical, .. } | BlockMapping::Unwritten { physical, .. } => {
                physical
                    .get()
                    .checked_add(1)
                    .map(FilesystemBlock::new)
                    .ok_or(Ext4Error::Overflow)
                    .map(Some)
            }
            BlockMapping::Hole { .. } => Ok(None),
        }
    }

    fn collect_allocated_write_runs(
        &self,
        inode: &Ext4Inode,
        offset: u64,
        total: usize,
        block_size_usize: usize,
    ) -> Ext4Result<Vec<AllocatedWriteRun>> {
        let block_size = u64::try_from(block_size_usize).map_err(|_| Ext4Error::Overflow)?;
        let mut input_offset = 0usize;
        let mut runs = Vec::new();

        while input_offset < total {
            let absolute = offset
                .checked_add(u64::try_from(input_offset).map_err(|_| Ext4Error::Overflow)?)
                .ok_or(Ext4Error::Overflow)?;
            let logical = absolute / block_size;
            let in_block =
                usize::try_from(absolute % block_size).map_err(|_| Ext4Error::Overflow)?;
            match self.map_blocks(inode, LogicalBlock::new(logical))? {
                BlockMapping::Hole { .. } | BlockMapping::Unwritten { .. } => {
                    return Err(Ext4Error::Unsupported(UnsupportedKind::UnallocatedWrite));
                }
                BlockMapping::Mapped { physical, len, .. } => {
                    if physical.get() == 0 || len.get() == 0 {
                        return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
                    }

                    let max_blocks = usize::try_from(len.get()).map_err(|_| Ext4Error::Overflow)?;
                    let max_bytes = max_blocks
                        .checked_mul(block_size_usize)
                        .and_then(|bytes| bytes.checked_sub(in_block))
                        .ok_or(Ext4Error::Overflow)?;
                    let write_len = (total - input_offset).min(max_bytes);
                    let covered_bytes =
                        write_len.checked_add(in_block).ok_or(Ext4Error::Overflow)?;
                    let rounded_bytes = covered_bytes
                        .checked_add(block_size_usize.checked_sub(1).ok_or(Ext4Error::Overflow)?)
                        .ok_or(Ext4Error::Overflow)?;
                    let block_count = rounded_bytes
                        .checked_div(block_size_usize)
                        .ok_or(Ext4Error::Overflow)?;
                    let block_count_u32 =
                        u32::try_from(block_count).map_err(|_| Ext4Error::Overflow)?;

                    runs.push(AllocatedWriteRun {
                        first_block: FilesystemBlock::new(physical.get()),
                        block_count: block_count_u32,
                        input_offset,
                        in_block,
                        write_len,
                    });
                    input_offset = input_offset
                        .checked_add(write_len)
                        .ok_or(Ext4Error::Overflow)?;
                }
            }
        }

        Ok(runs)
    }

    fn write_allocated_run(
        &self,
        run: &AllocatedWriteRun,
        input: &[u8],
        block_size_usize: usize,
        partial_block_write: PartialBlockWrite,
    ) -> Ext4Result<()> {
        let input_end = run
            .input_offset
            .checked_add(run.write_len)
            .ok_or(Ext4Error::Overflow)?;
        let input_slice = input
            .get(run.input_offset..input_end)
            .ok_or(Ext4Error::OutOfBounds)?;
        let block_count = usize::try_from(run.block_count).map_err(|_| Ext4Error::Overflow)?;
        let is_full_aligned_write =
            run.in_block == 0 && run.write_len == block_count * block_size_usize;
        if is_full_aligned_write {
            return self.write_contiguous_blocks(run.first_block, run.block_count, input_slice);
        }

        let mut block_bytes = vec![
            0;
            block_count
                .checked_mul(block_size_usize)
                .ok_or(Ext4Error::Overflow)?
        ];
        match partial_block_write {
            PartialBlockWrite::PreserveExisting => {
                self.read_blocks(run.first_block, run.block_count, &mut block_bytes)?;
            }
            PartialBlockWrite::ZeroFill => {}
        }
        let target_end = run
            .in_block
            .checked_add(run.write_len)
            .ok_or(Ext4Error::Overflow)?;
        block_bytes
            .get_mut(run.in_block..target_end)
            .ok_or(Ext4Error::OutOfBounds)?
            .copy_from_slice(input_slice);
        self.write_contiguous_blocks(run.first_block, run.block_count, &block_bytes)
    }
}

fn validate_ordered_writeback_mapping(mapping: BlockMapping) -> Ext4Result<u32> {
    match mapping {
        BlockMapping::Mapped { physical, len, .. }
        | BlockMapping::Unwritten { physical, len, .. } => {
            if physical.get() == 0 || len.get() == 0 {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
            }
            Ok(len.get())
        }
        BlockMapping::Hole { len, .. } => {
            if len.get() == 0 {
                return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
            }
            Ok(len.get())
        }
    }
}

fn normalize_write_allocation_len(requested: u32, hole_len: u32, has_stream_goal: bool) -> u32 {
    if requested < EXT4_PREALLOC_MIN_WRITE_BLOCKS || hole_len <= requested {
        return requested;
    }
    let target = if has_stream_goal {
        requested.saturating_mul(EXT4_STREAM_PREALLOC_MULTIPLIER)
    } else {
        requested.max(EXT4_RANDOM_PREALLOC_BLOCKS)
    };
    target
        .max(requested)
        .min(hole_len)
        .min(EXT4_MAX_PREALLOC_BLOCKS)
}

fn mapping_write_len(
    remaining_input: usize,
    mapping_blocks: u32,
    in_block: usize,
    block_size_usize: usize,
) -> Ext4Result<usize> {
    let mapped_bytes = usize::try_from(mapping_blocks)
        .map_err(|_| Ext4Error::Overflow)?
        .checked_mul(block_size_usize)
        .and_then(|bytes| bytes.checked_sub(in_block))
        .ok_or(Ext4Error::Overflow)?;
    let write_len = remaining_input.min(mapped_bytes);
    if write_len == 0 {
        return Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent));
    }
    Ok(write_len)
}

fn block_count_for_byte_span(
    in_block: usize,
    byte_len: usize,
    block_size_usize: usize,
) -> Ext4Result<u32> {
    let covered_bytes = byte_len.checked_add(in_block).ok_or(Ext4Error::Overflow)?;
    let rounded_bytes = covered_bytes
        .checked_add(block_size_usize.checked_sub(1).ok_or(Ext4Error::Overflow)?)
        .ok_or(Ext4Error::Overflow)?;
    let block_count = rounded_bytes
        .checked_div(block_size_usize)
        .ok_or(Ext4Error::Overflow)?;
    u32::try_from(block_count).map_err(|_| Ext4Error::Overflow)
}

#[cfg(test)]
mod tests {
    use super::normalize_write_allocation_len;

    #[test]
    fn normalized_allocation_keeps_single_block_writes_exact() {
        assert_eq!(normalize_write_allocation_len(1, 64, true), 1);
        assert_eq!(normalize_write_allocation_len(1, 64, false), 1);
    }

    #[test]
    fn normalized_allocation_expands_streaming_requests_within_hole() {
        assert_eq!(normalize_write_allocation_len(4, 128, true), 32);
        assert_eq!(normalize_write_allocation_len(4, 10, true), 10);
    }

    #[test]
    fn normalized_allocation_caps_random_requests() {
        assert_eq!(normalize_write_allocation_len(2, 128, false), 2);
        assert_eq!(normalize_write_allocation_len(4, 128, false), 8);
        assert_eq!(normalize_write_allocation_len(512, 4096, true), 1024);
    }
}

#[cfg(unittest)]
mod unittests {
    use unittest::{assert_eq, def_test};

    use super::*;
    use crate::extent::BlockMappingFlags;

    #[def_test]
    fn hole_writeback_credits_grow_by_actual_allocation_run() {
        let hole = BlockMapping::Hole {
            len: BlockCount::new(8),
            flags: BlockMappingFlags::empty(),
        };
        let first = OrderedWritebackCreditUsage::default().with_mapping_operation(hole);
        assert_eq!(first.and_then(|usage| usage.credit_bound()), Ok(75));
        assert_eq!(
            first
                .and_then(|usage| usage.with_mapping_operation(hole))
                .and_then(|usage| usage.credit_bound()),
            Ok(142)
        );
    }

    #[def_test]
    fn fragmented_unwritten_credits_follow_actual_conversion_count() {
        let mut usage = OrderedWritebackCreditUsage::default();
        for logical in 0..8u64 {
            let mapping = if logical % 2 == 0 {
                BlockMapping::Mapped {
                    physical: PhysicalBlock::new(100 + logical),
                    len: BlockCount::new(1),
                    flags: BlockMappingFlags::empty(),
                }
            } else {
                BlockMapping::Unwritten {
                    physical: PhysicalBlock::new(100 + logical),
                    len: BlockCount::new(1),
                    flags: BlockMappingFlags::empty(),
                }
            };
            usage = usage.with_mapping_operation(mapping).unwrap();
        }
        assert_eq!(usage.credit_bound(), Ok(136));
    }

    #[def_test]
    fn writeback_cursor_carries_preallocation_budget_across_transactions() {
        let mut cursor = OrderedWritebackCursor::new(12);
        assert_eq!(cursor.advance(4096), Ok(()));
        cursor.remaining_extra_prealloc_blocks -= 3;
        cursor.settle();
        assert_eq!(cursor.input_offset, 4096);
        assert_eq!(cursor.settled_input_offset, 4096);
        assert_eq!(cursor.remaining_extra_prealloc_blocks, 9);
    }

    #[def_test]
    fn mapped_writeback_does_not_grow_metadata_operation_credits() {
        let mapped = BlockMapping::Mapped {
            physical: PhysicalBlock::new(100),
            len: BlockCount::new(4),
            flags: BlockMappingFlags::empty(),
        };
        assert_eq!(
            OrderedWritebackCreditUsage::default()
                .with_mapping_operation(mapped)
                .map(|usage| (usage.allocation_runs, usage.extent_mutations)),
            Ok((0, 0))
        );
    }
}

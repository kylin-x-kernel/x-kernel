// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Stable physical preparation and ordered publication after owned data I/O.

use alloc::{sync::Arc, vec, vec::Vec};

use super::{
    AllocatedWriteRun, OrderedWritebackData, PartialBlockWrite, block_count_for_byte_span,
    mapping_write_len, validate_ordered_writeback_mapping,
};
use crate::{
    BlockMapping, Ext4Error, Ext4Inode, Ext4Result, Ext4SbInfo, Ext4SyncIntent, Ext4WritebackError,
    Ext4WritebackResult, FilesystemBlock, LogicalBlock, UnsupportedKind, jbd2::JournalCredits,
};

struct PreparedRun {
    mapping: AllocatedWriteRun,
    partial: PartialBlockWrite,
}

/// No journal handle or mount guard crosses the data-I/O boundary. Newly
/// allocated blocks remain unwritten until successful completion is settled.
/// The inode submission/drain protocol protects all physical mappings.
pub(crate) struct PreparedWriteback {
    device: Arc<crate::io::FilesystemDevice>,
    runs: Vec<PreparedRun>,
}

impl PreparedWriteback {
    pub(super) fn mapped(
        device: Arc<crate::io::FilesystemDevice>,
        runs: Vec<AllocatedWriteRun>,
    ) -> Self {
        Self {
            device,
            runs: runs
                .into_iter()
                .map(|mapping| PreparedRun {
                    mapping,
                    partial: PartialBlockWrite::PreserveExisting,
                })
                .collect(),
        }
    }

    /// Prepares all buffers and callbacks before device ownership starts.
    /// Partial initialized blocks are read outside mount protection; unwritten
    /// blocks are zero padded, including the unused tail of an appended page.
    /// Preparation failures call `record_error` before dropping `finish` and its
    /// cache completion. Neither callback runs in BIO end-I/O context.
    pub(crate) fn submit(
        self,
        input: Vec<u8>,
        queue: &Arc<crate::writeback_io::InodeWritebackIo>,
        finish: impl Fn(Ext4WritebackResult) + Send + Sync + 'static,
        record_error: impl Fn(Ext4Error),
    ) -> Ext4Result<()> {
        let io = queue
            .prepare(self.runs.iter().map(|run| run.mapping.write_len).collect())
            .inspect_err(|&error| record_error(error))?;
        io.set_finish(finish);
        let prepared = self
            .prepare_bios(input, &io)
            .inspect_err(|&error| record_error(error))?;
        // Retain io here so an install failure records its error before the
        // completion capture releases/redirties any cache folio.
        queue
            .install(io.clone())
            .inspect_err(|&error| record_error(error))?;
        for bio in prepared {
            self.device.submit_owned_write(bio);
        }
        Ok(())
    }

    fn prepare_bios(
        &self,
        input: Vec<u8>,
        io: &Arc<crate::writeback_io::WritebackIo>,
    ) -> Ext4Result<Vec<block::Bio>> {
        let mut prepared = Vec::with_capacity(self.runs.len());
        let block_size = self.device.block_size();
        if self.runs.len() == 1
            && self.runs[0].mapping.in_block == 0
            && input.len().is_multiple_of(block_size)
        {
            let completion = io.clone();
            prepared.push(self.device.prepare_owned_write(
                self.runs[0].mapping.first_block,
                input.into_boxed_slice(),
                move |result, buffers| completion.end_io(0, (result, buffers)),
            )?);
        } else {
            for (index, prepared_run) in self.runs.iter().enumerate() {
                let run = &prepared_run.mapping;
                let bytes = (run.block_count as usize)
                    .checked_mul(block_size)
                    .ok_or(Ext4Error::Overflow)?;
                let mut data = vec![0; bytes];
                if (run.in_block != 0 || run.write_len != bytes)
                    && matches!(prepared_run.partial, PartialBlockWrite::PreserveExisting)
                {
                    self.device
                        .read_blocks(run.first_block, run.block_count, &mut data)?;
                }
                data[run.in_block..run.in_block + run.write_len]
                    .copy_from_slice(&input[run.input_offset..run.input_offset + run.write_len]);
                let completion = io.clone();
                prepared.push(self.device.prepare_owned_write(
                    run.first_block,
                    data.into_boxed_slice(),
                    move |result, buffers| completion.end_io(index, (result, buffers)),
                )?);
            }
        }
        Ok(prepared)
    }
}

impl Ext4SbInfo {
    /// Installs unwritten extents for holes without publishing a larger disk
    /// size or initialized extents. Bounded allocation transactions can commit
    /// safely before data I/O. Caller excludes mapping removal until drain.
    pub(crate) fn prepare_allocating_writeback(
        &mut self,
        inode: &Arc<Ext4Inode>,
        offset: u64,
        total: usize,
        visible_size: u64,
        intent: Ext4SyncIntent,
    ) -> Ext4Result<PreparedWriteback> {
        self.ensure_regular_file_mutation_supported(inode)?;
        let end = offset
            .checked_add(total as u64)
            .ok_or(Ext4Error::Overflow)?;
        if total == 0 || end > visible_size || visible_size < inode.disk_size() {
            return Err(Ext4Error::OutOfBounds);
        }
        if self
            .journal
            .as_ref()
            .is_some_and(|journal| journal.is_aborted())
        {
            return Err(Ext4Error::JournalAborted);
        }
        if self.journal.is_none() && (intent.requires_full_metadata() || end > inode.disk_size()) {
            return Err(Ext4Error::Unsupported(UnsupportedKind::JournaledWrite));
        }
        let block_size = self.device.block_size();
        let mut input_offset = 0;
        let mut runs = Vec::new();
        let mut extra_blocks = self.unreserved_blocks_count().min(u64::from(u32::MAX)) as u32;
        while input_offset < total {
            let absolute = offset
                .checked_add(input_offset as u64)
                .ok_or(Ext4Error::Overflow)?;
            let logical = LogicalBlock::new(absolute / block_size as u64);
            let in_block = (absolute % block_size as u64) as usize;
            let mapping = self.map_blocks(inode, logical)?;
            validate_ordered_writeback_mapping(mapping)?;
            let (physical, count, partial) = match mapping {
                BlockMapping::Mapped { physical, len, .. } => {
                    (physical, len, PartialBlockWrite::PreserveExisting)
                }
                BlockMapping::Unwritten { physical, len, .. } => {
                    if self.journal.is_none() {
                        return Err(Ext4Error::Unsupported(UnsupportedKind::JournaledWrite));
                    }
                    (physical, len, PartialBlockWrite::ZeroFill)
                }
                BlockMapping::Hole { len, .. } => {
                    let requested =
                        block_count_for_byte_span(in_block, total - input_offset, block_size)?
                            .min(len.get())
                            .min(u32::from(crate::disk::extent::EXT_UNWRITTEN_MAX_LEN));
                    let credits =
                        JournalCredits::new(self.extent_writeback_metadata_credits(inode, 1, 1)?);
                    let required_blocks = if self.has_pending_block_frees()? {
                        extra_blocks = 0;
                        self.ordered_writeback_allocation_blocks(inode, logical, requested)?
                    } else {
                        0
                    };
                    let journal = self.metadata_journal_for_allocation(
                        credits,
                        crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
                        required_blocks,
                    )?;
                    let mut handle = journal.begin(credits)?;
                    let allocated = self.allocate_unwritten_writeback_run(
                        inode,
                        logical,
                        requested,
                        len.get(),
                        &mut extra_blocks,
                        &mut handle,
                    );
                    let (physical, count) = self.complete_metadata_mutation(handle, allocated)?;
                    // The bitmap and unwritten extent now own these blocks.
                    // A failed data write retries that mapping, so keeping its
                    // hole reservation would charge the same space twice.
                    self.release_delalloc_range(inode, logical, u64::from(count.get()))?;
                    (physical, count, PartialBlockWrite::ZeroFill)
                }
            };
            let write_len =
                mapping_write_len(total - input_offset, count.get(), in_block, block_size)?;
            runs.push(PreparedRun {
                mapping: AllocatedWriteRun {
                    first_block: FilesystemBlock::new(physical.get()),
                    block_count: block_count_for_byte_span(in_block, write_len, block_size)?,
                    input_offset,
                    in_block,
                    write_len,
                },
                partial,
            });
            input_offset += write_len;
        }
        Ok(PreparedWriteback {
            device: self.device.clone(),
            runs,
        })
    }

    /// Converts only successfully written data and publishes at most its end.
    /// The existing ordered engine owns credits, barriers, transaction restart,
    /// committed-prefix reporting and delalloc release; it issues no data I/O.
    pub(crate) fn finish_prepared_writeback(
        &mut self,
        inode: &Arc<Ext4Inode>,
        offset: u64,
        data: Ext4WritebackResult,
        intent: Ext4SyncIntent,
        is_open_for_write: impl Fn() -> bool,
    ) -> Ext4WritebackResult {
        let bytes = match data {
            Ok(bytes) => bytes,
            Err(failure) if intent.requires_full_metadata() => {
                return Err(self.fail_metadata_mutation(false, failure.error()).into());
            }
            Err(failure) => failure.completed_bytes(),
        };
        if bytes == 0 {
            return data;
        }
        let size = inode.disk_size().max(
            offset
                .checked_add(bytes as u64)
                .ok_or(Ext4Error::Overflow)?,
        );
        let settled = self.writeback_ordered_data(
            inode,
            offset,
            OrderedWritebackData::Completed(bytes),
            size,
            intent,
            0,
            is_open_for_write,
        );
        match data {
            Ok(_) => settled,
            Err(failure) => {
                let prefix = match settled {
                    Ok(bytes) => bytes,
                    Err(metadata) => {
                        error!(
                            "KExt4 inode {} failed to settle writeback at {offset}: {:?}",
                            inode.number().get(),
                            metadata.error()
                        );
                        metadata.completed_bytes()
                    }
                };
                Err(Ext4WritebackError::new(prefix, failure.error()))
            }
        }
    }
}

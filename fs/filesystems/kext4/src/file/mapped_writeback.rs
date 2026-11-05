// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Completion for stable, already initialized mappings without extent mutation.

use alloc::{sync::Arc, vec::Vec};

use super::{AllocatedWriteRun, OrderedWritebackCursor, RegularWriteMetadata};
use crate::{
    Ext4Error, Ext4Inode, Ext4Result, Ext4SbInfo, Ext4SyncIntent, Ext4WritebackError,
    Ext4WritebackResult, jbd2::JournalCredits,
};

/// Fully mapped, aligned overwrite prepared while mount state is protected.
/// Submission protection lasts through caller-owned completion, or until the
/// request is transferred to the inode queue for deferred completion.
/// No journal handle or borrowed data buffer survives mapping preparation.
pub(crate) struct MappedWriteback {
    device: Arc<crate::io::FilesystemDevice>,
    runs: Vec<AllocatedWriteRun>,
}

/// Bounded owned BIOs, drained in their preparing task on every exit.
/// Drop waits for remaining I/O before a surrounding PageCache completion can
/// redirty/release the corresponding folios. No mount lock may be held then.
pub(crate) struct PendingMappedWriteback {
    requests: Vec<(block::completion::BioWait, usize)>,
}

impl MappedWriteback {
    pub(super) fn new(
        device: Arc<crate::io::FilesystemDevice>,
        runs: Vec<AllocatedWriteRun>,
    ) -> Self {
        Self { device, runs }
    }

    /// Transfers the same validated mappings to worker-owned completion.
    pub(crate) fn into_async(self) -> super::PreparedWriteback {
        super::PreparedWriteback::mapped(self.device, self.runs)
    }

    /// Submits without waiting; the preparing task must drain the result outside
    /// mount protection before releasing inode submission protection.
    pub(crate) fn submit(self, input: Vec<u8>) -> Ext4Result<PendingMappedWriteback> {
        // Prepare every request before publishing any; preparation failure has
        // no partial device side effects. Keep the common contiguous path copy-free.
        let mut prepared = Vec::with_capacity(self.runs.len());
        if self.runs.len() == 1 {
            let (bio, wait) = self
                .device
                .prepare_waited_write(self.runs[0].first_block, input.into_boxed_slice())?;
            prepared.push((bio, wait, self.runs[0].write_len));
        } else {
            for run in self.runs {
                let data = input[run.input_offset..run.input_offset + run.write_len]
                    .to_vec()
                    .into_boxed_slice();
                let (bio, wait) = self.device.prepare_waited_write(run.first_block, data)?;
                prepared.push((bio, wait, run.write_len));
            }
        }
        let mut requests = Vec::with_capacity(prepared.len());
        for (bio, wait, bytes) in prepared {
            self.device.submit_owned_write(bio);
            requests.push((wait, bytes));
        }
        Ok(PendingMappedWriteback { requests })
    }
}

impl PendingMappedWriteback {
    /// Waits every BIO, even after an error; reports only the contiguous prefix.
    pub(crate) fn wait(mut self) -> Ext4WritebackResult {
        let mut completed_bytes = 0;
        let mut first_error = None;
        for (wait, bytes) in self.requests.drain(..) {
            let (result, _buffers) = wait.wait();
            if first_error.is_none() {
                match result {
                    Ok(()) => completed_bytes += bytes,
                    Err(error) => first_error = Some(Ext4Error::Device(error)),
                }
            }
        }
        first_error.map_or(Ok(completed_bytes), |error| {
            Err(Ext4WritebackError::new(completed_bytes, error))
        })
    }
}

impl Drop for PendingMappedWriteback {
    fn drop(&mut self) {
        for (wait, _) in self.requests.drain(..) {
            let _ = wait.wait();
        }
    }
}

impl Ext4SbInfo {
    /// Settles an overwrite prepared by `prepare_mapped_writeback`. Submission
    /// protection keeps its initialized mappings and disk size stable until
    /// completion; metadata is read live so concurrent timestamp changes survive.
    pub(crate) fn finish_mapped_writeback(
        &mut self,
        inode: &Ext4Inode,
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
        let settled =
            self.complete_mapped_writeback(inode, offset, bytes, intent, is_open_for_write);
        match data {
            Ok(_) => settled,
            Err(failure) => Err(Ext4WritebackError::new(
                settled.unwrap_or_else(|error| error.completed_bytes()),
                failure.error(),
            )),
        }
    }

    /// Publishes metadata only after successful data I/O, with the same ordered
    /// flush dependency and allocation admission as the synchronous path.
    /// Re-read the writable-open gate at settlement so in-use preallocations
    /// survive both successful overwrites and cleanup of failed data prefixes.
    fn complete_mapped_writeback(
        &mut self,
        inode: &Ext4Inode,
        offset: u64,
        completed_bytes: usize,
        intent: Ext4SyncIntent,
        is_open_for_write: impl Fn() -> bool,
    ) -> Ext4WritebackResult {
        if intent.requires_full_metadata() {
            let credits = JournalCredits::new(self.extent_writeback_metadata_credits(inode, 0, 0)?);
            let journal = self.metadata_journal_for_allocation(
                credits,
                crate::journal::RecoveryFlagPolicy::ClearAfterCheckpoint,
                self.inode_extra_isize_allocation_blocks(inode),
            )?;
            let mut handle = journal.begin(credits)?;
            let result = handle.require_ordered_data_flush().and_then(|()| {
                self.update_regular_inode_write_metadata(
                    inode,
                    inode.disk_size(),
                    RegularWriteMetadata::SizeOnly,
                    &mut handle,
                )
            });
            self.complete_metadata_mutation(handle, result)?;
        }
        let mut cursor = OrderedWritebackCursor::new(0);
        cursor.advance(completed_bytes)?;
        self.release_ordered_writeback_cursor_range(
            inode,
            offset,
            self.device.block_size(),
            &cursor,
            &is_open_for_write,
        )
        .map_err(|error| cursor.current_failure(error))?;
        Ok(completed_bytes)
    }
}

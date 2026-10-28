// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Bounded BIO submission and task-context completion for mapped overwrites.

use alloc::{sync::Arc, vec::Vec};

use kvfs::{AddressSpace, SuperBlock, VfsError, VfsInode, VfsResult, WritebackControl};
use pagecache::{WritebackBatch, WritebackCompletion};

use super::{Ext4Mount, MAX_WRITEBACK_BYTES, ext4_private, into_vfs_err};
use crate::{Ext4Inode, Ext4SyncIntent, file::PendingMappedWriteback};

// Four PageCache batches bound retained snapshots and queued BIOs per inode.
const MAX_PENDING_BATCHES: usize = 4;

struct PendingBatch<'a> {
    // Field order also drains I/O before fallback completion during Drop.
    io: PendingMappedWriteback,
    completion: WritebackCompletion<'a>,
    offset: u64,
}

struct WritebackPass<'a> {
    ext4: &'a Ext4Mount,
    super_block: &'a SuperBlock,
    vfs_inode: &'a VfsInode,
    inode: &'a Arc<Ext4Inode>,
    intent: Ext4SyncIntent,
    pending: Vec<PendingBatch<'a>>,
}

impl<'a> WritebackPass<'a> {
    fn submit(&mut self, batch: WritebackBatch<'a>) -> VfsResult<()> {
        let offset = batch.offset();
        let plan = self
            .ext4
            .read()
            .prepare_mapped_writeback(
                self.inode,
                offset,
                batch.data(),
                self.vfs_inode.size(),
                self.intent,
            )
            .map_err(into_vfs_err)?;
        if let Some(plan) = plan {
            let (data, completion) = batch.into_parts();
            let io = plan.submit(data).map_err(into_vfs_err)?;
            self.pending.push(PendingBatch {
                io,
                completion,
                offset,
            });
            if self.pending.len() == MAX_PENDING_BATCHES {
                self.drain()?;
            }
            return Ok(());
        }

        // Allocation/unwritten/partial-block paths retain their existing
        // transaction protocol. Do not run it ahead of earlier in-flight data.
        self.drain()?;
        let result = self.ext4.write(self.super_block).writeback_ordered_at(
            self.inode,
            offset,
            batch.data(),
            self.vfs_inode.size(),
            self.intent,
            || self.vfs_inode.write_count() != 0,
        );
        let (_, completion) = batch.into_parts();
        finish_cache_batch(self.inode, offset, completion, result)
    }

    fn drain(&mut self) -> VfsResult<()> {
        let mut first_error = None;
        for pending in self.pending.drain(..) {
            let result = pending.io.wait();
            let result = match result {
                Ok(bytes) => self.ext4.write(self.super_block).finish_mapped_writeback(
                    self.inode,
                    pending.offset,
                    bytes,
                    self.intent,
                    || self.vfs_inode.write_count() != 0,
                ),
                Err(failure) => {
                    let mut state = self.ext4.write(self.super_block);
                    if self.intent.requires_full_metadata() {
                        // No metadata publication is allowed for failed data.
                        let error = state.fail_metadata_mutation(false, failure.error());
                        Err(error.into())
                    } else {
                        // Release reservations for the successful prefix, but
                        // preserve the original device error if cleanup also fails.
                        let settlement = state.finish_mapped_writeback(
                            self.inode,
                            pending.offset,
                            failure.completed_bytes(),
                            self.intent,
                            || self.vfs_inode.write_count() != 0,
                        );
                        let prefix = match settlement {
                            Ok(bytes) => bytes,
                            Err(error) => {
                                error!(
                                    "KExt4 inode {} failed to settle writeback at {}: {:?}",
                                    self.inode.number().get(),
                                    pending.offset,
                                    error.error()
                                );
                                // Keep the batch dirty when reservation settlement
                                // failed; do not discard the original I/O error.
                                0
                            }
                        };
                        Err(crate::file::Ext4WritebackError::new(
                            prefix,
                            failure.error(),
                        ))
                    }
                }
            };
            if let Err(error) =
                finish_cache_batch(self.inode, pending.offset, pending.completion, result)
            {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

fn finish_cache_batch(
    inode: &Ext4Inode,
    offset: u64,
    completion: WritebackCompletion<'_>,
    result: crate::file::Ext4WritebackResult,
) -> VfsResult<()> {
    let (prefix, error) = match result {
        Ok(bytes) => (bytes, None),
        Err(failure) => {
            error!(
                "KExt4 inode {} writeback at offset {offset} failed after {} bytes: {:?}",
                inode.number().get(),
                failure.completed_bytes(),
                failure.error()
            );
            (
                failure.completed_bytes(),
                Some(into_vfs_err(failure.error())),
            )
        }
    };
    let settled = completion.complete(prefix);
    // Preserve the filesystem/device root cause if cache settlement also fails.
    error.map_or_else(|| settled.map(|_| ()), Err)
}

pub(super) fn writepages(mapping: &AddressSpace, control: &mut WritebackControl) -> VfsResult<()> {
    let vfs_inode = mapping.inode().ok_or(VfsError::InvalidInput)?;
    let inode = vfs_inode.downcast::<Ext4Inode>()?;
    let _writeback_guard = inode.lock_writeback();
    let super_block = vfs_inode.super_block()?;
    let mut pass = WritebackPass {
        ext4: ext4_private(&super_block)?,
        super_block: &super_block,
        vfs_inode: &vfs_inode,
        inode: &inode,
        intent: Ext4SyncIntent::from_data_only(control.is_data_only()),
        pending: Vec::with_capacity(MAX_PENDING_BATCHES),
    };
    let submitted =
        mapping.submit_cached_ranges(control, MAX_WRITEBACK_BYTES, |batch| pass.submit(batch));
    // Both modes drain in this first integration. No deferred error escapes a
    // writepages call, and invalidate/inode guards outlive every completion.
    let completed = pass.drain();
    completed.and(submitted)
}

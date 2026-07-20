// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use super::{
    access::{MetadataBuffer, MetadataWriteAccess, MetadataWriteback},
    cache::MetadataBlockCache,
};
use crate::{
    Ext4Result, FilesystemBlock,
    io::FilesystemDevice,
    jbd2::{JournalCommit, JournalCommitBlock, JournalHandle, JournalUndo, TransactionId},
};

/// Ext4-local adapter for physical metadata I/O.
///
/// Ordinary file contents do not pass through this object. They remain owned
/// by the KFS `FileMapping`/page-cache path once the VFS adapter is connected.
pub(crate) struct Ext4MetadataIo {
    pub(super) cache: MetadataBlockCache,
}

impl Ext4MetadataIo {
    pub(crate) fn new(device: Arc<FilesystemDevice>) -> Self {
        Self {
            cache: MetadataBlockCache::new(device),
        }
    }

    /// Reads one physical block containing ext4 or JBD2 metadata.
    ///
    /// This operation may sleep and must be called from task context.
    pub(crate) fn read_block(&self, block: FilesystemBlock) -> Ext4Result<MetadataBuffer> {
        self.cache.read(block)
    }

    /// Obtains journal write access to one metadata block.
    ///
    /// The returned token pins the cache entry and records transaction
    /// ownership. The journal handle supplies credits and abort checks before
    /// the buffer state can transition away from clean.
    #[allow(dead_code)]
    pub(crate) fn write_access(
        &self,
        block: FilesystemBlock,
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<MetadataWriteAccess> {
        handle.consume_metadata_credit(block)?;
        match self.cache.write_access(block, handle.id()) {
            Ok(access) => Ok(access),
            Err(error) => {
                handle.refund_metadata_credit(block);
                Err(error)
            }
        }
    }

    /// Obtains journal write access and records the pre-transaction image.
    #[allow(dead_code)]
    pub(crate) fn undo_access(
        &self,
        block: FilesystemBlock,
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<MetadataWriteAccess> {
        handle.consume_metadata_credit(block)?;
        match self.cache.undo_access(block, handle.id()) {
            Ok((access, committed)) => {
                if let Err(error) = handle.record_undo_block(block, committed.into_bytes()) {
                    handle.refund_metadata_credit(block);
                    return Err(error);
                }
                Ok(access)
            }
            Err(error) => {
                handle.refund_metadata_credit(block);
                Err(error)
            }
        }
    }

    /// Obtains journal create access to one newly allocated metadata block.
    ///
    /// The block is not read from disk. The caller must fully initialize the
    /// buffer contents before the transaction can be committed.
    #[allow(dead_code)]
    pub(crate) fn create_access(
        &self,
        block: FilesystemBlock,
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<MetadataWriteAccess> {
        handle.consume_metadata_credit(block)?;
        match self.cache.create_access(block, handle.id()) {
            Ok(access) => Ok(access),
            Err(error) => {
                handle.refund_metadata_credit(block);
                Err(error)
            }
        }
    }

    /// Records that a freed metadata block must suppress older journal replay.
    #[allow(dead_code)]
    pub(crate) fn forget_metadata_block(
        &self,
        block: FilesystemBlock,
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<()> {
        handle.ensure_revoke_metadata_credit(block)?;
        let revoke = handle.revoke_metadata_block(block)?;
        if let Err(error) = self.cache.forget_or_revoke_checkpointed(block, handle.id()) {
            handle.cancel_revoke_metadata_block(block, revoke);
            return Err(error);
        }
        Ok(())
    }

    /// Drops cached/checkpointed metadata state and the handle's pending update
    /// without emitting a JBD2 revoke.
    ///
    /// The caller must guarantee that no older uncheckpointed transaction can
    /// replay this block after it is freed and reused.
    pub(crate) fn forget_metadata_block_without_revoke(
        &self,
        block: FilesystemBlock,
        handle: &mut JournalHandle<'_>,
    ) -> Ext4Result<()> {
        let forget = handle.forget_metadata_block_without_revoke(block)?;
        if let Err(error) = self.cache.forget_or_revoke_checkpointed(block, handle.id()) {
            handle.cancel_forget_metadata_block_without_revoke(block, forget);
            return Err(error);
        }
        Ok(())
    }

    /// Moves a dirty metadata buffer into checkpoint writeback.
    #[allow(dead_code)]
    pub(crate) fn start_writeback(
        &self,
        block: FilesystemBlock,
        transaction: TransactionId,
    ) -> Ext4Result<MetadataWriteback> {
        self.cache.start_writeback(block, transaction)
    }

    /// Checkpoints metadata buffers touched by a committed transaction.
    ///
    /// Dirty buffers are written to their home blocks and clean journaled
    /// buffers are released. The flush at the end is the stable-storage
    /// boundary that later `fsync` and `syncfs` code can build on.
    #[allow(dead_code)]
    pub(crate) fn checkpoint_committed(&self, commit: &JournalCommit) -> Ext4Result<()> {
        for block in commit.metadata_blocks() {
            self.cache.checkpoint_block(*block, commit.id())?;
        }
        self.cache.flush_device()
    }

    /// Captures metadata images that must be emitted into an online JBD2 commit.
    #[allow(dead_code)]
    pub(crate) fn journal_commit_blocks(
        &self,
        commit: &JournalCommit,
    ) -> Ext4Result<alloc::vec::Vec<JournalCommitBlock>> {
        commit
            .metadata_blocks()
            .iter()
            .map(|block| self.cache.journal_commit_block(*block, commit.id()))
            .collect()
    }

    pub(crate) fn reclaim_unused(&self, limit: usize) -> usize {
        self.cache.reclaim_unused(limit)
    }

    pub(crate) fn invalidate_all(&self) {
        self.cache.invalidate_all();
    }

    #[allow(dead_code)]
    pub(crate) fn rollback_undo(&self, undo: &JournalUndo) -> Ext4Result<()> {
        self.cache.rollback_undo(undo)
    }
}

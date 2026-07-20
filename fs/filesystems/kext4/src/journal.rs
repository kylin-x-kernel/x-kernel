// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Filesystem-level JBD2 commit, checkpoint, and recovery glue.

use alloc::{sync::Arc, vec};

use block::BlockDevice;

use crate::{
    disk::{Superblock, superblock},
    error::{CorruptKind, Ext4Error, Ext4Result, UnsupportedKind},
    jbd2::{
        Journal, JournalBlock, JournalBlockMapper, JournalBlockReader, JournalBlockWriter,
        JournalCommit, JournalCommitBlock, JournalPersistedCommit, JournalReplayApplied,
        JournalReplayReport, JournalSuperblock, TransactionId, finish_journal_checkpoint,
        mark_superblock_empty, persist_journal_commit,
    },
    superblock::{
        Ext4Filesystem, Ext4Recovery, Ext4RecoveryCleared, Ext4RecoveryReport, JournalMarkedEmpty,
    },
    types::FilesystemBlock,
};

const JBD2_FEATURE_INCOMPAT_REVOKE: u32 = 0x0000_0001;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RecoveryFlagPolicy {
    ClearAfterCheckpoint,
    PreserveDuringRecovery,
}

pub(crate) struct PendingMetadataCheckpoint {
    journal: Journal,
    commit: JournalCommit,
    persisted: JournalPersistedCommit,
    recovery_flag_policy: RecoveryFlagPolicy,
}

impl Ext4Filesystem {
    /// Returns whether the mounted journal can persist revoke records.
    pub(crate) fn journal_supports_revoke(&self) -> bool {
        self.journal.as_ref().is_some_and(|journal| {
            journal.superblock.feature_incompat() & JBD2_FEATURE_INCOMPAT_REVOKE != 0
        })
    }

    /// Synchronizes filesystem-owned state with stable storage.
    ///
    /// Metadata mutations enqueue committed journal records for checkpointing.
    /// Until a kernel background worker is wired in, mutating callers drive the
    /// queue synchronously. `syncfs` drains any remaining work through the
    /// current KVFS hook; freeze/unmount should call the same drain path once
    /// those lifecycle hooks exist in the shared KVFS layer.
    pub fn sync_filesystem(&mut self) -> Ext4Result<()> {
        self.drain_pending_checkpoints()?;
        self.flush_device()?;
        let _ = self.metadata_io.reclaim_unused(usize::MAX);
        Ok(())
    }

    /// Flushes and cleans filesystem-owned state before unmount.
    pub fn unmount_filesystem(&mut self) -> Ext4Result<()> {
        self.drain_pending_checkpoints()?;
        self.sync_filesystem()?;
        self.cleanup_legacy_orphans()?;
        self.drain_pending_checkpoints()?;
        self.sync_filesystem()
    }

    pub(crate) fn metadata_journal(&mut self) -> Ext4Result<Journal> {
        self.drain_pending_checkpoints()?;
        let sequence = self
            .journal
            .as_ref()
            .ok_or(Ext4Error::Unsupported(UnsupportedKind::JournaledWrite))?
            .superblock
            .sequence();
        Ok(Journal::new(sequence))
    }

    pub(crate) fn commit_metadata_transaction(
        &mut self,
        journal: Journal,
        transaction: TransactionId,
    ) -> Ext4Result<()> {
        self.commit_metadata_transaction_with_policy(
            journal,
            transaction,
            RecoveryFlagPolicy::ClearAfterCheckpoint,
        )
    }

    pub(crate) fn commit_metadata_transaction_with_policy(
        &mut self,
        journal: Journal,
        transaction: TransactionId,
        recovery_flag_policy: RecoveryFlagPolicy,
    ) -> Ext4Result<()> {
        self.enqueue_metadata_checkpoint(journal, transaction, recovery_flag_policy)?;
        self.run_checkpoint_worker()
    }

    #[cfg(test)]
    pub(crate) fn pending_checkpoint_count(&self) -> usize {
        self.pending_checkpoints.len()
    }

    pub(crate) fn drain_pending_checkpoints(&mut self) -> Ext4Result<()> {
        while !self.pending_checkpoints.is_empty() {
            self.run_checkpoint_worker()?;
        }
        Ok(())
    }

    fn enqueue_metadata_checkpoint(
        &mut self,
        journal: Journal,
        transaction: TransactionId,
        recovery_flag_policy: RecoveryFlagPolicy,
    ) -> Ext4Result<()> {
        self.drain_pending_checkpoints()?;
        let commit = journal.force_commit(transaction)?;
        let persisted = self.persist_metadata_journal_commit(&commit)?;
        self.pending_checkpoints
            .push_back(PendingMetadataCheckpoint {
                journal,
                commit,
                persisted,
                recovery_flag_policy,
            });
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn enqueue_metadata_checkpoint_for_test(
        &mut self,
        journal: Journal,
        transaction: TransactionId,
    ) -> Ext4Result<()> {
        self.enqueue_metadata_checkpoint(
            journal,
            transaction,
            RecoveryFlagPolicy::ClearAfterCheckpoint,
        )
    }

    fn run_checkpoint_worker(&mut self) -> Ext4Result<()> {
        let Some(pending) = self.pending_checkpoints.pop_front() else {
            return Ok(());
        };
        if let Err(error) = self.checkpoint_metadata_journal_commit_with_policy(
            &pending.commit,
            &pending.persisted,
            pending.recovery_flag_policy,
        ) {
            self.pending_checkpoints.push_front(pending);
            return Err(error);
        }
        if let Err(error) = pending.journal.finish_checkpoint(&pending.commit) {
            self.pending_checkpoints.push_front(pending);
            return Err(error);
        }
        let _ = self.metadata_io.reclaim_unused(usize::MAX);
        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) fn persist_metadata_journal_commit(
        &mut self,
        commit: &JournalCommit,
    ) -> Ext4Result<JournalPersistedCommit> {
        self.set_ext4_needs_recovery_feature()?;
        let superblock = self
            .journal
            .as_ref()
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidJournal))?
            .superblock
            .clone();
        let mut blocks = self.metadata_io.journal_commit_blocks(commit)?;
        self.merge_recovery_feature_into_journaled_superblock(&mut blocks)?;
        let persisted = match persist_journal_commit(&superblock, self, commit, &blocks) {
            Ok(persisted) => persisted,
            Err(error) => {
                self.reset_block_allocation_caches();
                return Err(error);
            }
        };
        self.journal
            .as_mut()
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidJournal))?
            .superblock = persisted.superblock().clone();
        Ok(persisted)
    }

    fn merge_recovery_feature_into_journaled_superblock(
        &self,
        blocks: &mut [JournalCommitBlock],
    ) -> Ext4Result<()> {
        let (superblock_block, superblock_offset, superblock_len) =
            self.primary_superblock_location()?;
        let superblock_end = superblock_offset
            .checked_add(superblock_len)
            .ok_or(Ext4Error::Overflow)?;
        for block in blocks.iter_mut() {
            if block.target() != superblock_block {
                continue;
            }
            let mut bytes = block.bytes().to_vec();
            let superblock_bytes = bytes
                .get_mut(superblock_offset..superblock_end)
                .ok_or(Ext4Error::OutOfBounds)?;
            superblock::set_ext4_needs_recovery_feature(superblock_bytes)?;
            *block = JournalCommitBlock::new(superblock_block, Arc::from(bytes.into_boxed_slice()));
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) fn finish_metadata_journal_checkpoint(
        &mut self,
        persisted: &JournalPersistedCommit,
    ) -> Ext4Result<()> {
        self.finish_metadata_journal_checkpoint_with_policy(
            persisted,
            RecoveryFlagPolicy::ClearAfterCheckpoint,
        )
    }

    pub(crate) fn finish_metadata_journal_checkpoint_with_policy(
        &mut self,
        persisted: &JournalPersistedCommit,
        recovery_flag_policy: RecoveryFlagPolicy,
    ) -> Ext4Result<()> {
        let superblock = self
            .journal
            .as_ref()
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidJournal))?
            .superblock
            .clone();
        let clean = match finish_journal_checkpoint(&superblock, self, persisted) {
            Ok(clean) => clean,
            Err(error) => {
                self.reset_block_allocation_caches();
                return Err(error);
            }
        };
        self.journal
            .as_mut()
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidJournal))?
            .superblock = clean;
        self.ensure_journal_superblock_has_zero_start()?;
        let state_result = match recovery_flag_policy {
            RecoveryFlagPolicy::ClearAfterCheckpoint => {
                self.clear_ext4_needs_recovery_feature_on_disk()
            }
            RecoveryFlagPolicy::PreserveDuringRecovery => {
                // Setting the recovery bit reads the pre-checkpoint superblock
                // from disk. Rebase after the home blocks land so the next
                // orphan sees the checkpointed head and allocator counters.
                self.reload_mutable_metadata_state()
            }
        };
        if let Err(error) = state_result {
            self.reset_block_allocation_caches();
            return Err(error);
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) fn checkpoint_metadata_journal_commit(
        &mut self,
        commit: &JournalCommit,
        persisted: &JournalPersistedCommit,
    ) -> Ext4Result<()> {
        self.checkpoint_metadata_journal_commit_with_policy(
            commit,
            persisted,
            RecoveryFlagPolicy::ClearAfterCheckpoint,
        )
    }

    pub(crate) fn checkpoint_metadata_journal_commit_with_policy(
        &mut self,
        commit: &JournalCommit,
        persisted: &JournalPersistedCommit,
        recovery_flag_policy: RecoveryFlagPolicy,
    ) -> Ext4Result<()> {
        if let Err(error) = self.metadata_io.checkpoint_committed(commit) {
            self.reset_block_allocation_caches();
            return Err(error);
        }
        self.finish_metadata_journal_checkpoint_with_policy(persisted, recovery_flag_policy)
    }

    fn mark_journal_clean(
        &mut self,
        applied: JournalReplayApplied,
    ) -> Ext4Result<JournalMarkedEmpty> {
        let report = applied.into_report();
        let (physical, bytes, superblock) = self.replayed_clean_journal_superblock(report)?;
        self.device.write_contiguous_blocks(physical, 1, &bytes)?;
        self.flush_device()?;
        self.journal
            .as_mut()
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidJournal))?
            .superblock = superblock;
        Ok(JournalMarkedEmpty { report })
    }

    fn stage_replayed_journal_clean_state(
        &mut self,
        applied: JournalReplayApplied,
    ) -> Ext4Result<()> {
        let (_, _, superblock) = self.replayed_clean_journal_superblock(applied.report())?;
        self.journal
            .as_mut()
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidJournal))?
            .superblock = superblock;
        Ok(())
    }

    fn replayed_clean_journal_superblock(
        &self,
        report: JournalReplayReport,
    ) -> Ext4Result<(FilesystemBlock, alloc::vec::Vec<u8>, JournalSuperblock)> {
        let (physical, block_count) = {
            let journal = self
                .journal
                .as_ref()
                .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidJournal))?;
            (
                journal.map_journal_block(JournalBlock::new(0))?,
                journal.block_count,
            )
        };
        let mut bytes = vec![0; self.device.block_size()];
        self.device.read_blocks(physical, 1, &mut bytes)?;
        mark_superblock_empty(&mut bytes, report.next_sequence(), report.head())?;
        let superblock = JournalSuperblock::decode(
            &bytes,
            self.layout.block_size(),
            block_count,
            self.superblock.uuid(),
        )?;
        Ok((physical, bytes, superblock))
    }

    fn clear_ext4_needs_recovery_feature(
        &mut self,
        marked: JournalMarkedEmpty,
    ) -> Ext4Result<Ext4RecoveryCleared> {
        self.ensure_journal_superblock_has_zero_start()?;
        self.clear_ext4_needs_recovery_feature_on_disk()?;
        Ok(Ext4RecoveryCleared {
            report: Ext4RecoveryReport::from_journal_report(marked.report),
        })
    }

    fn ensure_journal_superblock_has_zero_start(&self) -> Ext4Result<()> {
        let journal = self
            .journal
            .as_ref()
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidJournal))?;
        if journal.superblock.has_nonzero_log_start() {
            return Err(Ext4Error::JournalBusy);
        }
        Ok(())
    }

    fn set_ext4_needs_recovery_feature(&mut self) -> Ext4Result<()> {
        self.update_ext4_superblock_recovery_feature(superblock::set_ext4_needs_recovery_feature)
    }

    fn clear_ext4_needs_recovery_feature_on_disk(&mut self) -> Ext4Result<()> {
        self.update_ext4_superblock_recovery_feature(superblock::clear_ext4_needs_recovery_feature)
    }

    fn update_ext4_superblock_recovery_feature(
        &mut self,
        update: impl FnOnce(&mut [u8]) -> Ext4Result<()>,
    ) -> Ext4Result<()> {
        let (block, offset, len) = self.primary_superblock_location()?;
        let end = offset.checked_add(len).ok_or(Ext4Error::Overflow)?;

        let mut bytes = vec![0; self.device.block_size()];
        self.device.read_blocks(block, 1, &mut bytes)?;
        let superblock_bytes = bytes.get_mut(offset..end).ok_or(Ext4Error::OutOfBounds)?;
        update(superblock_bytes)?;
        let superblock = Superblock::decode(superblock_bytes)?;
        self.device.write_contiguous_blocks(block, 1, &bytes)?;
        self.flush_device()?;
        self.superblock = superblock;
        Ok(())
    }
}

impl Ext4Recovery {
    pub(super) fn open(device: Arc<dyn BlockDevice>) -> Ext4Result<Self> {
        Ok(Self {
            filesystem: Ext4Filesystem::open(device, true)?,
        })
    }

    pub(super) fn replay(mut self) -> Ext4Result<Option<Ext4RecoveryReport>> {
        let features = self.filesystem.superblock.features();
        if features.has_orphan_file() || features.has_orphan_present() {
            return Err(Ext4Error::Unsupported(UnsupportedKind::OrphanFile));
        }
        if !features.needs_recovery() {
            if self.filesystem.orphan_head().is_some() {
                self.filesystem.cleanup_legacy_orphans()?;
            }
            return Ok(None);
        }
        let applied = self.filesystem.replay_internal_journal_updates()?;
        self.filesystem.metadata_io.invalidate_all();
        self.filesystem.reload_mutable_metadata_state()?;
        self.filesystem
            .stage_replayed_journal_clean_state(applied)?;
        let cleaned_orphans = if self.filesystem.orphan_head().is_some() {
            self.filesystem
                .cleanup_legacy_orphans_preserving_recovery()?
        } else {
            0
        };
        let marked = if cleaned_orphans == 0 {
            self.filesystem.mark_journal_clean(applied)?
        } else {
            self.filesystem.ensure_journal_superblock_has_zero_start()?;
            JournalMarkedEmpty {
                report: applied.report(),
            }
        };
        let cleared = self.filesystem.clear_ext4_needs_recovery_feature(marked)?;
        let report = cleared.into_report();
        Ok(Some(report))
    }
}

impl JournalBlockMapper for Ext4Filesystem {
    fn map_journal_block(&self, block: JournalBlock) -> Ext4Result<FilesystemBlock> {
        self.journal
            .as_ref()
            .ok_or(Ext4Error::OutOfBounds)?
            .map_journal_block(block)
    }
}

impl JournalBlockReader for Ext4Filesystem {
    fn read_journal_block(&self, block: JournalBlock, output: &mut [u8]) -> Ext4Result<()> {
        let expected =
            usize::try_from(self.layout.block_size()).map_err(|_| Ext4Error::Overflow)?;
        if output.len() != expected {
            return Err(Ext4Error::InvalidBufferLength {
                expected,
                actual: output.len(),
            });
        }
        let physical = self.map_journal_block(block)?;
        self.device.read_blocks(physical, 1, output)
    }
}

impl JournalBlockWriter for Ext4Filesystem {
    fn write_journal_block(&self, block: JournalBlock, input: &[u8]) -> Ext4Result<()> {
        let expected =
            usize::try_from(self.layout.block_size()).map_err(|_| Ext4Error::Overflow)?;
        if input.len() != expected {
            return Err(Ext4Error::InvalidBufferLength {
                expected,
                actual: input.len(),
            });
        }
        let physical = self.map_journal_block(block)?;
        self.device.write_contiguous_blocks(physical, 1, input)
    }

    fn flush_journal(&self) -> Ext4Result<()> {
        self.flush_device()
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Filesystem-level JBD2 commit, checkpoint, and recovery glue.

use alloc::{collections::BTreeMap, sync::Arc, vec};
#[cfg(not(target_os = "none"))]
use std::sync::{Mutex, MutexGuard};

use block::BlockDeviceOperations;
#[cfg(target_os = "none")]
use ksync::{Mutex, MutexGuard};

use crate::{
    disk::{Superblock, superblock},
    error::{CorruptKind, Ext4Error, Ext4Result, UnsupportedKind},
    jbd2::{
        JournalBlock, JournalBlockMapper, JournalBlockReader, JournalBlockWriter,
        JournalCommitBlock, JournalCredits, JournalHandle, JournalPersistedCommit,
        JournalReplayApplied, JournalReplayReport, JournalSuperblock, JournalTransactions,
        RuntimeTransaction, TransactionId, enable_superblock_revoke, finish_journal_checkpoint,
        mark_superblock_empty, persist_journal_commit,
    },
    superblock::{
        Ext4Filesystem, Ext4Recovery, Ext4RecoveryCleared, Ext4RecoveryReport, InternalJournal,
        JournalMarkedEmpty,
    },
    types::FilesystemBlock,
};

const JBD2_FEATURE_INCOMPAT_REVOKE: u32 = 0x0000_0001;

fn transaction_credit_limit(superblock: &JournalSuperblock) -> Ext4Result<u32> {
    let log_blocks = superblock
        .max_blocks()
        .checked_sub(superblock.first_log_block().get())
        .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidJournal))?;
    // Linux bounds a normal transaction below roughly one third of the
    // journal so descriptor, revoke, and commit overhead still fit while a
    // checkpoint can make progress.
    Ok((log_blocks / 3).max(1))
}

#[cfg(target_os = "none")]
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock()
}

#[cfg(not(target_os = "none"))]
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RecoveryFlagPolicy {
    ClearAfterCheckpoint,
    PreserveDuringRecovery,
}

/// Journal state whose lifetime matches one mounted ext4 filesystem.
///
/// This is the production journal identity. The transaction engine is private
/// implementation state borrowed by handles; commit and checkpoint entry
/// points always resolve it through this mount-owned object.
pub(crate) struct MountedJournal {
    transactions: JournalTransactions,
    state: Mutex<MountedJournalState>,
}

struct MountedJournalState {
    storage: InternalJournal,
    checkpoint_policies: BTreeMap<TransactionId, RecoveryFlagPolicy>,
}

impl MountedJournal {
    pub(crate) fn new(
        storage: InternalJournal,
        filesystem_block_count: u64,
    ) -> Ext4Result<Arc<Self>> {
        storage.validate_physical_bounds(filesystem_block_count)?;
        let transactions = JournalTransactions::new_with_credit_limit(
            storage.superblock.sequence(),
            transaction_credit_limit(&storage.superblock)?,
        );
        Ok(Arc::new(Self {
            transactions,
            state: Mutex::new(MountedJournalState {
                storage,
                checkpoint_policies: BTreeMap::new(),
            }),
        }))
    }

    pub(crate) fn superblock(&self) -> JournalSuperblock {
        lock(&self.state).storage.superblock.clone()
    }

    fn supports_revoke(&self) -> bool {
        lock(&self.state).storage.superblock.feature_incompat() & JBD2_FEATURE_INCOMPAT_REVOKE != 0
    }

    fn replace_superblock(&self, superblock: JournalSuperblock) {
        lock(&self.state).storage.superblock = superblock;
    }

    #[cfg(test)]
    fn pending_checkpoint_count(&self) -> usize {
        lock(&self.state).checkpoint_policies.len()
    }

    fn has_pending_checkpoints(&self) -> bool {
        self.transactions.has_checkpoint_transactions()
    }

    fn next_checkpoint_persisted_commit(&self) -> Ext4Result<Option<JournalPersistedCommit>> {
        self.transactions.next_checkpoint_persisted_commit()
    }

    fn newest_persisted_commit(&self) -> Ext4Result<Option<JournalPersistedCommit>> {
        self.transactions.newest_persisted_commit()
    }

    fn insert_checkpoint_policy(
        &self,
        transaction: TransactionId,
        policy: RecoveryFlagPolicy,
    ) -> Ext4Result<()> {
        let replaced = lock(&self.state)
            .checkpoint_policies
            .insert(transaction, policy);
        if replaced.is_some() {
            return Err(Ext4Error::InvalidJournalTransaction);
        }
        Ok(())
    }

    fn checkpoint_policy(&self, transaction: TransactionId) -> Ext4Result<RecoveryFlagPolicy> {
        lock(&self.state)
            .checkpoint_policies
            .get(&transaction)
            .copied()
            .ok_or(Ext4Error::InvalidJournalTransaction)
    }

    fn remove_checkpoint_policy(&self, transaction: TransactionId) -> Ext4Result<()> {
        if lock(&self.state)
            .checkpoint_policies
            .remove(&transaction)
            .is_none()
        {
            return Err(Ext4Error::InvalidJournalTransaction);
        }
        Ok(())
    }

    fn map_block(&self, block: JournalBlock) -> Ext4Result<FilesystemBlock> {
        lock(&self.state).storage.map_journal_block(block)
    }

    fn block_count(&self) -> u32 {
        lock(&self.state).storage.block_count
    }

    fn reset_after_replay(
        &self,
        superblock: JournalSuperblock,
        first_transaction: TransactionId,
    ) -> Ext4Result<()> {
        let mut state = lock(&self.state);
        if !state.checkpoint_policies.is_empty() {
            return Err(Ext4Error::JournalBusy);
        }
        self.transactions.reset_after_replay(first_transaction)?;
        state.storage.superblock = superblock;
        Ok(())
    }

    pub(crate) fn begin(&self, credits: JournalCredits) -> Ext4Result<JournalHandle<'_>> {
        self.transactions.begin(credits)
    }

    #[cfg(test)]
    pub(crate) fn is_aborted(&self) -> bool {
        self.transactions.is_aborted()
    }

    #[cfg(test)]
    pub(crate) fn running_transaction(&self) -> Ext4Result<Option<TransactionId>> {
        self.transactions.running_transaction()
    }

    #[cfg(test)]
    pub(crate) fn force_commit_for_test(
        &self,
        transaction: TransactionId,
    ) -> Ext4Result<Arc<RuntimeTransaction>> {
        self.transactions.force_commit(transaction)
    }
}

impl Ext4Filesystem {
    /// Returns whether the mounted journal can persist revoke records.
    pub(crate) fn journal_supports_revoke(&self) -> bool {
        self.journal
            .as_ref()
            .is_some_and(|journal| journal.supports_revoke())
    }

    /// Synchronizes filesystem-owned state with stable storage.
    ///
    /// Metadata mutations enqueue committed journal records for checkpointing.
    /// Until a kernel background worker is wired in, `syncfs`, KVFS unmount
    /// writeback, and journal-space pressure drive the queue. Freeze should
    /// call the same drain path once that lifecycle hook exists in KVFS.
    pub fn sync_filesystem(&mut self) -> Ext4Result<()> {
        let result = (|| {
            self.commit_running_metadata_transaction()?;
            self.drain_pending_checkpoints()?;
            self.flush_device()?;
            let _ = self.metadata_io.reclaim_unused(usize::MAX);
            Ok(())
        })();
        result.map_err(|error| self.fail_journal_operation(error))
    }

    pub(crate) fn metadata_journal(&mut self) -> Ext4Result<Arc<MountedJournal>> {
        if !self.journal_supports_revoke() {
            self.drain_pending_checkpoints()?;
            self.enable_journal_revoke_feature()?;
        }
        self.journal
            .as_ref()
            .cloned()
            .ok_or(Ext4Error::Unsupported(UnsupportedKind::JournaledWrite))
    }

    pub(crate) fn metadata_journal_for_mutation(
        &mut self,
        credits: JournalCredits,
        recovery_flag_policy: RecoveryFlagPolicy,
    ) -> Ext4Result<Arc<MountedJournal>> {
        let journal = self.metadata_journal()?;
        if recovery_flag_policy == RecoveryFlagPolicy::PreserveDuringRecovery {
            self.commit_running_metadata_transaction_with_policy(recovery_flag_policy)?;
            self.drain_pending_checkpoints()?;
            return Ok(journal);
        }

        if let Some(transaction) = journal
            .transactions
            .transaction_to_commit_before_reservation(credits)?
        {
            self.commit_metadata_transaction(transaction)?;
        }
        Ok(journal)
    }

    fn enable_journal_revoke_feature(&mut self) -> Ext4Result<()> {
        if self.journal_supports_revoke() {
            return Ok(());
        }
        let superblock = self
            .journal
            .as_ref()
            .ok_or(Ext4Error::Unsupported(UnsupportedKind::JournaledWrite))?
            .superblock();
        if superblock.has_nonzero_log_start() {
            return Ok(());
        }

        let mut bytes = superblock.encoded().to_vec();
        if !enable_superblock_revoke(&mut bytes)? {
            return Ok(());
        }
        self.write_journal_block(JournalBlock::new(0), &bytes)?;
        self.flush_journal()?;
        let updated = JournalSuperblock::decode(
            &bytes,
            superblock.block_size(),
            superblock.max_blocks(),
            superblock.uuid(),
        )?;
        self.journal
            .as_ref()
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidJournal))?
            .replace_superblock(updated);
        Ok(())
    }

    pub(crate) fn commit_metadata_transaction(
        &mut self,
        transaction: TransactionId,
    ) -> Ext4Result<()> {
        self.commit_metadata_transaction_with_policy(
            transaction,
            RecoveryFlagPolicy::ClearAfterCheckpoint,
        )
    }

    pub(crate) fn commit_metadata_transaction_with_policy(
        &mut self,
        transaction: TransactionId,
        recovery_flag_policy: RecoveryFlagPolicy,
    ) -> Ext4Result<()> {
        let requires_synchronous_checkpoint = !self.journal_supports_revoke()
            || recovery_flag_policy == RecoveryFlagPolicy::PreserveDuringRecovery;
        self.append_metadata_checkpoint(transaction, recovery_flag_policy)?;
        if requires_synchronous_checkpoint {
            self.run_checkpoint_worker()?;
        }
        Ok(())
    }

    pub(crate) fn finish_metadata_mutation_with_policy(
        &mut self,
        transaction: TransactionId,
        recovery_flag_policy: RecoveryFlagPolicy,
    ) -> Ext4Result<()> {
        let journal = self
            .journal
            .as_ref()
            .cloned()
            .ok_or(Ext4Error::Unsupported(UnsupportedKind::JournaledWrite))?;
        let requires_immediate_commit = !self.journal_supports_revoke()
            || recovery_flag_policy == RecoveryFlagPolicy::PreserveDuringRecovery;
        let transaction_to_commit = if requires_immediate_commit {
            journal.transactions.idle_running_transaction(transaction)?
        } else {
            journal
                .transactions
                .transaction_to_commit_after_handle(transaction)?
        };
        if let Some(transaction) = transaction_to_commit {
            self.commit_metadata_transaction_with_policy(transaction, recovery_flag_policy)?;
        }
        Ok(())
    }

    pub(crate) fn complete_metadata_mutation<T>(
        &mut self,
        handle: JournalHandle<'_>,
        result: Ext4Result<T>,
    ) -> Ext4Result<T> {
        self.complete_metadata_mutation_with_policy(
            handle,
            result,
            RecoveryFlagPolicy::ClearAfterCheckpoint,
        )
    }

    pub(crate) fn complete_metadata_mutation_with_policy<T>(
        &mut self,
        handle: JournalHandle<'_>,
        result: Ext4Result<T>,
        recovery_flag_policy: RecoveryFlagPolicy,
    ) -> Ext4Result<T> {
        let transaction = handle.id();
        let has_updates = handle.has_updates();
        if let Err(error) = handle.stop() {
            return Err(self.fail_metadata_mutation(has_updates, error));
        }
        match result {
            Ok(value) => {
                match self.finish_metadata_mutation_with_policy(transaction, recovery_flag_policy) {
                    Ok(()) => Ok(value),
                    Err(error) => Err(self.fail_metadata_mutation(has_updates, error)),
                }
            }
            Err(error) => Err(self.fail_metadata_mutation(has_updates, error)),
        }
    }

    pub(crate) fn complete_metadata_mutation_and_commit_with_policy<T>(
        &mut self,
        handle: JournalHandle<'_>,
        result: Ext4Result<T>,
        recovery_flag_policy: RecoveryFlagPolicy,
    ) -> Ext4Result<T> {
        let transaction = handle.id();
        let has_updates = handle.has_updates();
        if let Err(error) = handle.stop() {
            return Err(self.fail_metadata_mutation(has_updates, error));
        }
        match result {
            Ok(value) => match self
                .commit_metadata_transaction_with_policy(transaction, recovery_flag_policy)
            {
                Ok(()) => Ok(value),
                Err(error) => Err(self.fail_metadata_mutation(has_updates, error)),
            },
            Err(error) => Err(self.fail_metadata_mutation(has_updates, error)),
        }
    }

    pub(crate) fn commit_running_metadata_transaction(&mut self) -> Ext4Result<bool> {
        self.commit_running_metadata_transaction_with_policy(
            RecoveryFlagPolicy::ClearAfterCheckpoint,
        )
    }

    fn commit_running_metadata_transaction_with_policy(
        &mut self,
        recovery_flag_policy: RecoveryFlagPolicy,
    ) -> Ext4Result<bool> {
        let Some(journal) = self.journal.as_ref().cloned() else {
            return Ok(false);
        };
        let Some(transaction) = journal.transactions.running_transaction()? else {
            return Ok(false);
        };
        self.commit_metadata_transaction_with_policy(transaction, recovery_flag_policy)?;
        Ok(true)
    }

    #[cfg(test)]
    pub(crate) fn pending_checkpoint_count(&self) -> usize {
        self.journal
            .as_ref()
            .map_or(0, |journal| journal.pending_checkpoint_count())
    }

    pub(crate) fn drain_pending_checkpoints(&mut self) -> Ext4Result<()> {
        while self
            .journal
            .as_ref()
            .is_some_and(|journal| journal.has_pending_checkpoints())
        {
            self.run_checkpoint_worker()?;
        }
        Ok(())
    }

    #[cfg(test)]
    fn enqueue_metadata_checkpoint(
        &mut self,
        transaction: TransactionId,
        recovery_flag_policy: RecoveryFlagPolicy,
    ) -> Ext4Result<()> {
        self.drain_pending_checkpoints()?;
        self.append_metadata_checkpoint(transaction, recovery_flag_policy)
    }

    fn append_metadata_checkpoint(
        &mut self,
        transaction: TransactionId,
        recovery_flag_policy: RecoveryFlagPolicy,
    ) -> Ext4Result<()> {
        let result = self.append_metadata_checkpoint_inner(transaction, recovery_flag_policy);
        result.map_err(|error| self.fail_journal_operation(error))
    }

    fn append_metadata_checkpoint_inner(
        &mut self,
        transaction: TransactionId,
        recovery_flag_policy: RecoveryFlagPolicy,
    ) -> Ext4Result<()> {
        let journal = self
            .journal
            .as_ref()
            .cloned()
            .ok_or(Ext4Error::Unsupported(UnsupportedKind::JournaledWrite))?;
        let commit = journal.transactions.force_commit(transaction)?;
        let persisted = loop {
            match self.persist_metadata_journal_commit(&commit) {
                Ok(persisted) => break persisted,
                Err(Ext4Error::JournalBusy)
                    if self
                        .journal
                        .as_ref()
                        .is_some_and(|journal| journal.has_pending_checkpoints()) =>
                {
                    self.run_checkpoint_worker()?;
                }
                Err(error) => return Err(error),
            }
        };
        journal.insert_checkpoint_policy(transaction, recovery_flag_policy)?;
        if let Err(error) = journal.transactions.start_checkpoint(&commit, persisted) {
            let _ = journal.remove_checkpoint_policy(transaction);
            return Err(error);
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn enqueue_metadata_checkpoint_for_test(
        &mut self,
        transaction: TransactionId,
    ) -> Ext4Result<()> {
        self.enqueue_metadata_checkpoint(transaction, RecoveryFlagPolicy::ClearAfterCheckpoint)
    }

    pub(crate) fn fail_metadata_mutation(
        &mut self,
        has_updates: bool,
        mutation_error: Ext4Error,
    ) -> Ext4Error {
        let requires_abort = mutation_error_requires_abort(mutation_error);
        if !requires_abort && !has_updates {
            return mutation_error;
        }
        let abort_error = if requires_abort {
            mutation_error
        } else {
            // A normal error after metadata publication means a prevalidation
            // or explicit-unwind invariant was missed. Treat that path as an
            // internal transaction failure instead of exposing partial state.
            Ext4Error::InvalidJournalTransaction
        };
        self.abort_metadata_journal(abort_error)
    }

    fn run_checkpoint_worker(&mut self) -> Ext4Result<()> {
        let result = self.run_checkpoint_worker_inner();
        result.map_err(|error| self.fail_journal_operation(error))
    }

    fn run_checkpoint_worker_inner(&mut self) -> Ext4Result<()> {
        let Some((commit, persisted)) = self
            .journal
            .as_ref()
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidJournal))?
            .transactions
            .checkpoint_transaction()?
        else {
            return Ok(());
        };
        let transaction = commit.id();
        let recovery_flag_policy = self
            .journal
            .as_ref()
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidJournal))?
            .checkpoint_policy(transaction)?;
        let next_oldest = self
            .journal
            .as_ref()
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidJournal))?
            .next_checkpoint_persisted_commit()?;
        self.checkpoint_metadata_journal_commit_with_policy(
            &commit,
            &persisted,
            next_oldest.as_ref(),
            recovery_flag_policy,
        )?;
        let journal = self
            .journal
            .as_ref()
            .cloned()
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidJournal))?;
        journal.transactions.finish_checkpoint(&commit)?;
        journal.remove_checkpoint_policy(transaction)?;
        let _ = self.metadata_io.reclaim_unused(usize::MAX);
        Ok(())
    }

    pub(crate) fn fail_journal_operation(&self, error: Ext4Error) -> Ext4Error {
        if mutation_error_requires_abort(error) {
            self.abort_metadata_journal(error)
        } else {
            error
        }
    }

    fn abort_metadata_journal(&self, error: Ext4Error) -> Ext4Error {
        if let Some(journal) = self.journal.as_ref() {
            journal.transactions.abort(error);
        }
        error
    }

    #[cfg(test)]
    pub(crate) fn run_checkpoint_worker_for_test(&mut self) -> Ext4Result<()> {
        self.run_checkpoint_worker()
    }

    #[allow(dead_code)]
    pub(crate) fn persist_metadata_journal_commit(
        &mut self,
        commit: &Arc<RuntimeTransaction>,
    ) -> Ext4Result<JournalPersistedCommit> {
        let journal = self
            .journal
            .as_ref()
            .cloned()
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidJournal))?;
        journal.transactions.validate_committing(commit)?;
        let previous = journal.newest_persisted_commit()?;
        self.set_ext4_needs_recovery_feature()?;
        let superblock = journal.superblock();
        let mut blocks = self.metadata_io.journal_commit_blocks(commit)?;
        self.merge_recovery_feature_into_journaled_superblock(commit.id(), &mut blocks)?;
        let (persisted, active_superblock) = match persist_journal_commit(
            &superblock,
            self,
            previous.as_ref(),
            commit.as_ref(),
            &blocks,
        ) {
            Ok(persisted) => persisted,
            Err(error) => {
                self.reset_block_allocation_caches();
                return Err(error);
            }
        };
        journal.replace_superblock(active_superblock);
        Ok(persisted)
    }

    fn merge_recovery_feature_into_journaled_superblock(
        &self,
        transaction: TransactionId,
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
            let bytes: Arc<[u8]> = Arc::from(bytes.into_boxed_slice());
            self.metadata_io.replace_checkpoint_bytes(
                superblock_block,
                transaction,
                bytes.clone(),
            )?;
            *block = JournalCommitBlock::new(superblock_block, bytes);
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
            None,
            RecoveryFlagPolicy::ClearAfterCheckpoint,
        )
    }

    pub(crate) fn finish_metadata_journal_checkpoint_with_policy(
        &mut self,
        persisted: &JournalPersistedCommit,
        next_oldest: Option<&JournalPersistedCommit>,
        recovery_flag_policy: RecoveryFlagPolicy,
    ) -> Ext4Result<()> {
        let superblock = self
            .journal
            .as_ref()
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidJournal))?
            .superblock();
        let advanced = match finish_journal_checkpoint(&superblock, self, persisted, next_oldest) {
            Ok(advanced) => advanced,
            Err(error) => {
                self.reset_block_allocation_caches();
                return Err(error);
            }
        };
        self.journal
            .as_ref()
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidJournal))?
            .replace_superblock(advanced);
        if next_oldest.is_some() {
            return Ok(());
        }
        self.ensure_journal_superblock_has_zero_start()?;
        let state_result = match recovery_flag_policy {
            RecoveryFlagPolicy::ClearAfterCheckpoint => {
                self.clear_ext4_needs_recovery_feature_on_disk()
            }
            RecoveryFlagPolicy::PreserveDuringRecovery => {
                // Setting the recovery bit reads the pre-checkpoint superblock
                // from disk. Rebase after the home blocks land so the next
                // orphan sees the checkpointed head and allocator counters.
                self.metadata_io.invalidate_all();
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
        commit: &Arc<RuntimeTransaction>,
        persisted: &JournalPersistedCommit,
    ) -> Ext4Result<()> {
        self.checkpoint_metadata_journal_commit_with_policy(
            commit,
            persisted,
            None,
            RecoveryFlagPolicy::ClearAfterCheckpoint,
        )
    }

    pub(crate) fn checkpoint_metadata_journal_commit_with_policy(
        &mut self,
        commit: &Arc<RuntimeTransaction>,
        persisted: &JournalPersistedCommit,
        next_oldest: Option<&JournalPersistedCommit>,
        recovery_flag_policy: RecoveryFlagPolicy,
    ) -> Ext4Result<()> {
        self.journal
            .as_ref()
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidJournal))?
            .transactions
            .validate_checkpoint(commit, persisted)?;
        if let Err(error) = self.metadata_io.checkpoint_committed(commit) {
            self.reset_block_allocation_caches();
            return Err(error);
        }
        self.finish_metadata_journal_checkpoint_with_policy(
            persisted,
            next_oldest,
            recovery_flag_policy,
        )
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
            .as_ref()
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidJournal))?
            .replace_superblock(superblock);
        Ok(JournalMarkedEmpty { report })
    }

    fn stage_replayed_journal_clean_state(
        &mut self,
        applied: JournalReplayApplied,
    ) -> Ext4Result<()> {
        let (_, _, superblock) = self.replayed_clean_journal_superblock(applied.report())?;
        self.journal
            .as_ref()
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidJournal))?
            .reset_after_replay(superblock, applied.report().next_sequence())?;
        Ok(())
    }

    fn replayed_clean_journal_superblock(
        &self,
        report: JournalReplayReport,
    ) -> Ext4Result<(FilesystemBlock, alloc::vec::Vec<u8>, JournalSuperblock)> {
        let (physical, block_count, mut bytes) = {
            let journal = self
                .journal
                .as_ref()
                .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidJournal))?;
            (
                journal.map_block(JournalBlock::new(0))?,
                journal.block_count(),
                journal.superblock().encoded().to_vec(),
            )
        };
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
        if journal.superblock().has_nonzero_log_start() {
            return Err(Ext4Error::JournalBusy);
        }
        Ok(())
    }

    fn set_ext4_needs_recovery_feature(&mut self) -> Ext4Result<()> {
        if self.superblock.features().needs_recovery() {
            return Ok(());
        }
        self.write_ext4_superblock_recovery_feature(superblock::set_ext4_needs_recovery_feature)?;
        // The disk copy still contains pre-checkpoint counters. Preserve the
        // newer in-memory metadata state while recording only the recovery bit.
        self.superblock.mark_needs_recovery();
        Ok(())
    }

    fn clear_ext4_needs_recovery_feature_on_disk(&mut self) -> Ext4Result<()> {
        self.superblock = self.write_ext4_superblock_recovery_feature(
            superblock::clear_ext4_needs_recovery_feature,
        )?;
        Ok(())
    }

    fn write_ext4_superblock_recovery_feature(
        &self,
        update: impl FnOnce(&mut [u8]) -> Ext4Result<()>,
    ) -> Ext4Result<Superblock> {
        let (block, offset, len) = self.primary_superblock_location()?;
        let end = offset.checked_add(len).ok_or(Ext4Error::Overflow)?;

        let mut bytes = vec![0; self.device.block_size()];
        self.device.read_blocks(block, 1, &mut bytes)?;
        let superblock_bytes = bytes.get_mut(offset..end).ok_or(Ext4Error::OutOfBounds)?;
        update(superblock_bytes)?;
        let superblock = Superblock::decode(superblock_bytes)?;
        self.device.write_contiguous_blocks(block, 1, &bytes)?;
        self.flush_device()?;
        Ok(superblock)
    }
}

fn mutation_error_requires_abort(error: Ext4Error) -> bool {
    matches!(
        error,
        Ext4Error::Device(_)
            | Ext4Error::JournalAborted
            | Ext4Error::InsufficientJournalCredits
            | Ext4Error::JournalBusy
            | Ext4Error::InvalidJournalTransaction
            | Ext4Error::ChecksumMismatch { .. }
            | Ext4Error::Corrupt(_)
            | Ext4Error::Unsupported(UnsupportedKind::ConcurrentMetadataTransaction)
    )
}

impl Ext4Recovery {
    pub(super) fn open(device: Arc<dyn BlockDeviceOperations>) -> Ext4Result<Self> {
        Ok(Self {
            filesystem: Ext4Filesystem::open(device, true)?,
        })
    }

    fn recover_clean_legacy_orphans(&mut self) -> Ext4Result<()> {
        self.filesystem
            .cleanup_legacy_orphans_preserving_recovery()?;
        self.filesystem.ensure_journal_superblock_has_zero_start()?;
        self.filesystem.clear_ext4_needs_recovery_feature_on_disk()
    }

    pub(super) fn replay(mut self) -> Ext4Result<Option<Ext4RecoveryReport>> {
        let features = self.filesystem.superblock.features();
        if features.has_orphan_file() || features.has_orphan_present() {
            return Err(Ext4Error::Unsupported(UnsupportedKind::OrphanFile));
        }
        if !features.needs_recovery() {
            if self.filesystem.orphan_head().is_some() {
                self.recover_clean_legacy_orphans()?;
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
            .map_block(block)
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

    fn write_journal_blocks(
        &self,
        start: JournalBlock,
        block_count: u32,
        input: &[u8],
    ) -> Ext4Result<()> {
        let block_size =
            usize::try_from(self.layout.block_size()).map_err(|_| Ext4Error::Overflow)?;
        let expected = usize::try_from(block_count)
            .map_err(|_| Ext4Error::Overflow)?
            .checked_mul(block_size)
            .ok_or(Ext4Error::Overflow)?;
        if input.len() != expected {
            return Err(Ext4Error::InvalidBufferLength {
                expected,
                actual: input.len(),
            });
        }
        if block_count == 0 {
            return Ok(());
        }

        let mut run_first_input_block = 0u32;
        let mut run_physical = self.map_journal_block(start)?;
        let mut previous_physical = run_physical;
        for index in 1..block_count {
            let logical = start
                .get()
                .checked_add(index)
                .map(JournalBlock::new)
                .ok_or(Ext4Error::Overflow)?;
            let physical = self.map_journal_block(logical)?;
            if previous_physical.get().checked_add(1) != Some(physical.get()) {
                self.write_journal_physical_run(
                    run_physical,
                    run_first_input_block,
                    index - run_first_input_block,
                    block_size,
                    input,
                )?;
                run_first_input_block = index;
                run_physical = physical;
            }
            previous_physical = physical;
        }
        self.write_journal_physical_run(
            run_physical,
            run_first_input_block,
            block_count - run_first_input_block,
            block_size,
            input,
        )
    }

    fn flush_journal(&self) -> Ext4Result<()> {
        self.flush_device()
    }
}

impl Ext4Filesystem {
    fn write_journal_physical_run(
        &self,
        physical: FilesystemBlock,
        first_input_block: u32,
        block_count: u32,
        block_size: usize,
        input: &[u8],
    ) -> Ext4Result<()> {
        let start = usize::try_from(first_input_block)
            .map_err(|_| Ext4Error::Overflow)?
            .checked_mul(block_size)
            .ok_or(Ext4Error::Overflow)?;
        let len = usize::try_from(block_count)
            .map_err(|_| Ext4Error::Overflow)?
            .checked_mul(block_size)
            .ok_or(Ext4Error::Overflow)?;
        let end = start.checked_add(len).ok_or(Ext4Error::Overflow)?;
        self.device.write_contiguous_blocks(
            physical,
            block_count,
            input.get(start..end).ok_or(Ext4Error::OutOfBounds)?,
        )
    }
}

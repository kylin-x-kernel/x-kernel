// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Minimal JBD2 transaction handle and credits state.
//!
//! This is not yet the descriptor/commit writer. It establishes the core
//! contract that metadata mutation must pass through a transaction handle with
//! credits and abort checks before touching a metadata buffer.

use alloc::{
    collections::{BTreeSet, VecDeque},
    sync::Arc,
    vec::Vec,
};
use core::fmt;
#[cfg(not(target_os = "none"))]
use std::sync::{Condvar, Mutex, MutexGuard};

#[cfg(target_os = "none")]
use ksync::{Mutex, MutexGuard};
#[cfg(target_os = "none")]
use ktask::WaitQueue;

use super::{JournalPersistedCommit, TransactionId};
use crate::{Ext4Error, Ext4Result, FilesystemBlock};

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

/// Metadata journal credits reserved for one operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JournalCredits(u32);

impl JournalCredits {
    /// Creates a credit count.
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    /// Returns the raw credit count.
    pub const fn get(self) -> u32 {
        self.0
    }

    const fn one() -> Self {
        Self(1)
    }
}

#[derive(Debug)]
struct RunningTransactionState {
    active_handles: u32,
    reserved_credits: u32,
    used_credits: u32,
    metadata_blocks: BTreeSet<FilesystemBlock>,
    revoked_blocks: BTreeSet<FilesystemBlock>,
}

impl RunningTransactionState {
    fn new() -> Self {
        Self {
            active_handles: 0,
            reserved_credits: 0,
            used_credits: 0,
            metadata_blocks: BTreeSet::new(),
            revoked_blocks: BTreeSet::new(),
        }
    }

    fn begin_handle(&mut self, credits: JournalCredits) -> Ext4Result<()> {
        let active_handles = self
            .active_handles
            .checked_add(1)
            .ok_or(Ext4Error::Overflow)?;
        let reserved_credits = self
            .reserved_credits
            .checked_add(credits.get())
            .ok_or(Ext4Error::Overflow)?;
        self.active_handles = active_handles;
        self.reserved_credits = reserved_credits;
        Ok(())
    }

    fn reserve(&mut self, credits: JournalCredits) -> Ext4Result<()> {
        self.reserved_credits = self
            .reserved_credits
            .checked_add(credits.get())
            .ok_or(Ext4Error::Overflow)?;
        Ok(())
    }

    fn consume(&mut self, credits: JournalCredits) -> Ext4Result<()> {
        self.used_credits = self
            .used_credits
            .checked_add(credits.get())
            .ok_or(Ext4Error::Overflow)?;
        Ok(())
    }

    fn refund(&mut self, credits: JournalCredits) -> Ext4Result<()> {
        self.used_credits = self
            .used_credits
            .checked_sub(credits.get())
            .ok_or(Ext4Error::InvalidJournalTransaction)?;
        Ok(())
    }

    fn freeze(self) -> FrozenTransactionState {
        FrozenTransactionState {
            reserved_credits: self.reserved_credits,
            used_credits: self.used_credits,
            metadata_blocks: Arc::from(
                self.metadata_blocks
                    .into_iter()
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
            ),
            revoked_blocks: Arc::from(
                self.revoked_blocks
                    .into_iter()
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
            ),
        }
    }

    fn finish_handle(&mut self, unused_credits: u32) -> Ext4Result<()> {
        let active_handles = self
            .active_handles
            .checked_sub(1)
            .ok_or(Ext4Error::InvalidJournalTransaction)?;
        let reserved_credits = self
            .reserved_credits
            .checked_sub(unused_credits)
            .ok_or(Ext4Error::InvalidJournalTransaction)?;
        self.active_handles = active_handles;
        self.reserved_credits = reserved_credits;
        Ok(())
    }

    fn remove_revoke_block(&mut self, block: FilesystemBlock) -> bool {
        self.revoked_blocks.remove(&block)
    }
}

#[derive(Debug)]
struct JournalInner {
    aborted: Option<Ext4Error>,
    next_transaction: TransactionId,
    running: Option<Arc<RuntimeTransaction>>,
    committing: Option<Arc<RuntimeTransaction>>,
    checkpoint_transactions: VecDeque<Arc<RuntimeTransaction>>,
    checkpointed_through: TransactionId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FrozenTransactionState {
    reserved_credits: u32,
    used_credits: u32,
    metadata_blocks: Arc<[FilesystemBlock]>,
    revoked_blocks: Arc<[FilesystemBlock]>,
}

#[derive(Debug)]
enum TransactionPhase {
    Running(RunningTransactionState),
    Committing(FrozenTransactionState),
    Checkpoint {
        frozen: FrozenTransactionState,
        persisted: Option<JournalPersistedCommit>,
    },
    Finished,
    Transitioning,
}

/// One mount-owned JBD2 runtime transaction.
///
/// The same object moves through running, committing, and checkpoint phases.
/// Freezing converts mutable membership sets into shared immutable slices; no
/// parallel commit object or transaction payload is created.
pub struct RuntimeTransaction {
    id: TransactionId,
    phase: Mutex<TransactionPhase>,
}

impl RuntimeTransaction {
    fn new(id: TransactionId) -> Self {
        Self {
            id,
            phase: Mutex::new(TransactionPhase::Running(RunningTransactionState::new())),
        }
    }

    /// Returns the transaction sequence.
    pub const fn id(&self) -> TransactionId {
        self.id
    }

    /// Returns the credits reserved by handles in this transaction.
    #[cfg(test)]
    pub fn reserved_credits(&self) -> Ext4Result<u32> {
        Ok(self.frozen_state()?.reserved_credits)
    }

    /// Returns credits consumed for metadata buffer write access.
    #[cfg(test)]
    pub fn used_credits(&self) -> Ext4Result<u32> {
        Ok(self.frozen_state()?.used_credits)
    }

    /// Returns metadata blocks touched by this transaction.
    pub fn metadata_blocks(&self) -> Ext4Result<Arc<[FilesystemBlock]>> {
        Ok(self.frozen_state()?.metadata_blocks)
    }

    /// Returns filesystem blocks revoked by this transaction.
    pub fn revoked_blocks(&self) -> Ext4Result<Arc<[FilesystemBlock]>> {
        Ok(self.frozen_state()?.revoked_blocks)
    }

    fn with_running<R>(
        &self,
        f: impl FnOnce(&mut RunningTransactionState) -> Ext4Result<R>,
    ) -> Ext4Result<R> {
        let mut phase = lock(&self.phase);
        let TransactionPhase::Running(running) = &mut *phase else {
            return Err(Ext4Error::InvalidJournalTransaction);
        };
        f(running)
    }

    fn running_state(&self) -> Ext4Result<(u32, u32)> {
        let phase = lock(&self.phase);
        let TransactionPhase::Running(running) = &*phase else {
            return Err(Ext4Error::InvalidJournalTransaction);
        };
        Ok((running.active_handles, running.reserved_credits))
    }

    fn freeze_for_commit(&self) -> Ext4Result<()> {
        let mut phase = lock(&self.phase);
        let current = core::mem::replace(&mut *phase, TransactionPhase::Transitioning);
        match current {
            TransactionPhase::Running(running) if running.active_handles == 0 => {
                *phase = TransactionPhase::Committing(running.freeze());
                Ok(())
            }
            TransactionPhase::Running(running) => {
                *phase = TransactionPhase::Running(running);
                Err(Ext4Error::JournalBusy)
            }
            other => {
                *phase = other;
                Err(Ext4Error::InvalidJournalTransaction)
            }
        }
    }

    fn start_checkpoint(&self, persisted: Option<JournalPersistedCommit>) -> Ext4Result<()> {
        let mut phase = lock(&self.phase);
        let current = core::mem::replace(&mut *phase, TransactionPhase::Transitioning);
        match current {
            TransactionPhase::Committing(frozen) => {
                *phase = TransactionPhase::Checkpoint { frozen, persisted };
                Ok(())
            }
            TransactionPhase::Checkpoint {
                frozen,
                persisted: existing,
            } if existing == persisted => {
                *phase = TransactionPhase::Checkpoint {
                    frozen,
                    persisted: existing,
                };
                Ok(())
            }
            other => {
                *phase = other;
                Err(Ext4Error::InvalidJournalTransaction)
            }
        }
    }

    fn finish_checkpoint(&self) -> Ext4Result<()> {
        let mut phase = lock(&self.phase);
        if !matches!(*phase, TransactionPhase::Checkpoint { .. }) {
            return Err(Ext4Error::InvalidJournalTransaction);
        }
        *phase = TransactionPhase::Finished;
        Ok(())
    }

    fn persisted_commit(&self) -> Ext4Result<Option<JournalPersistedCommit>> {
        let phase = lock(&self.phase);
        match &*phase {
            TransactionPhase::Checkpoint { persisted, .. } => Ok(*persisted),
            TransactionPhase::Running(_)
            | TransactionPhase::Committing(_)
            | TransactionPhase::Finished
            | TransactionPhase::Transitioning => Err(Ext4Error::InvalidJournalTransaction),
        }
    }

    fn frozen_state(&self) -> Ext4Result<FrozenTransactionState> {
        let phase = lock(&self.phase);
        match &*phase {
            TransactionPhase::Committing(frozen) | TransactionPhase::Checkpoint { frozen, .. } => {
                Ok(frozen.clone())
            }
            TransactionPhase::Running(_)
            | TransactionPhase::Finished
            | TransactionPhase::Transitioning => Err(Ext4Error::InvalidJournalTransaction),
        }
    }
}

impl fmt::Debug for RuntimeTransaction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RuntimeTransaction")
            .field("id", &self.id)
            .field("phase", &*lock(&self.phase))
            .finish()
    }
}

/// Evidence that a committed transaction has completed checkpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JournalCheckpoint {
    transaction: TransactionId,
}

impl JournalCheckpoint {
    /// Returns the checkpointed transaction sequence.
    #[cfg(test)]
    pub const fn transaction(&self) -> TransactionId {
        self.transaction
    }
}

/// In-memory JBD2 transaction engine owned by one mounted journal.
///
/// This private subobject owns transaction identity, credits, abort state, and
/// checkpoint ordering. `MountedJournal` drives persistence while the same
/// runtime transaction moves through the phases represented here.
pub(crate) struct JournalTransactions {
    #[cfg(target_os = "none")]
    checkpoint_waiters: WaitQueue,
    #[cfg(not(target_os = "none"))]
    checkpoint_condition: Condvar,
    max_reserved_credits: u32,
    inner: Mutex<JournalInner>,
}

impl fmt::Debug for JournalTransactions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inner = lock(&self.inner);
        f.debug_struct("JournalTransactions")
            .field("inner", &*inner)
            .finish_non_exhaustive()
    }
}

impl JournalTransactions {
    /// Creates an unbounded transaction engine for isolated tests.
    #[cfg(test)]
    pub const fn new(first_transaction: TransactionId) -> Self {
        Self::new_with_credit_limit(first_transaction, u32::MAX)
    }

    /// Creates the transaction engine for one mounted journal.
    pub(crate) const fn new_with_credit_limit(
        first_transaction: TransactionId,
        max_reserved_credits: u32,
    ) -> Self {
        Self {
            #[cfg(target_os = "none")]
            checkpoint_waiters: WaitQueue::new(),
            #[cfg(not(target_os = "none"))]
            checkpoint_condition: Condvar::new(),
            max_reserved_credits,
            inner: Mutex::new(JournalInner {
                aborted: None,
                next_transaction: first_transaction,
                running: None,
                committing: None,
                checkpoint_transactions: VecDeque::new(),
                checkpointed_through: previous_transaction(first_transaction),
            }),
        }
    }

    /// Reinitializes empty runtime transaction state after journal replay.
    ///
    /// Recovery advances the on-disk sequence independently of the engine
    /// created during mount. Resetting here keeps the next handle id aligned
    /// with the replay report before orphan cleanup starts a new transaction.
    pub(crate) fn reset_after_replay(&self, first_transaction: TransactionId) -> Ext4Result<()> {
        let mut inner = lock(&self.inner);
        if inner.running.is_some()
            || inner.committing.is_some()
            || !inner.checkpoint_transactions.is_empty()
        {
            return Err(Ext4Error::JournalBusy);
        }
        *inner = JournalInner {
            aborted: None,
            next_transaction: first_transaction,
            running: None,
            committing: None,
            checkpoint_transactions: VecDeque::new(),
            checkpointed_through: previous_transaction(first_transaction),
        };
        Ok(())
    }

    /// Starts or joins the running transaction.
    pub fn begin(&self, credits: JournalCredits) -> Ext4Result<JournalHandle<'_>> {
        if credits.get() > self.max_reserved_credits {
            return Err(Ext4Error::InsufficientJournalCredits);
        }
        let mut inner = lock(&self.inner);
        check_not_aborted(&inner)?;

        let id = match &mut inner.running {
            Some(transaction) => {
                transaction.with_running(|running| running.begin_handle(credits))?;
                transaction.id()
            }
            None => {
                let id = inner.next_transaction;
                let transaction = Arc::new(RuntimeTransaction::new(id));
                transaction.with_running(|running| running.begin_handle(credits))?;
                inner.next_transaction = TransactionId::new(id.get().wrapping_add(1));
                inner.running = Some(transaction);
                id
            }
        };

        Ok(JournalHandle {
            journal: self,
            id,
            remaining_credits: credits.get(),
            has_updates: false,
            closed: false,
        })
    }

    /// Returns an idle transaction that must be committed before reserving
    /// credits for another handle.
    ///
    /// Active handles may continue sharing a transaction while the projected
    /// reservation remains below the limit. Once the limit is reached, the
    /// caller must wait for those handles instead of freezing their
    /// transaction underneath them.
    pub(crate) fn transaction_to_commit_before_reservation(
        &self,
        incoming_credits: JournalCredits,
    ) -> Ext4Result<Option<TransactionId>> {
        if incoming_credits.get() > self.max_reserved_credits {
            return Err(Ext4Error::InsufficientJournalCredits);
        }
        let inner = lock(&self.inner);
        check_not_aborted(&inner)?;
        let Some(running) = inner.running.as_ref() else {
            return Ok(None);
        };
        let (active_handles, reserved_credits) = running.running_state()?;
        let projected_credits = reserved_credits
            .checked_add(incoming_credits.get())
            .ok_or(Ext4Error::Overflow)?;
        if projected_credits < self.max_reserved_credits {
            return Ok(None);
        }
        if active_handles != 0 {
            return Err(Ext4Error::JournalBusy);
        }
        Ok(Some(running.id()))
    }

    /// Returns this transaction after its last handle closes and its retained
    /// credits reach the batching limit.
    pub(crate) fn transaction_to_commit_after_handle(
        &self,
        transaction: TransactionId,
    ) -> Ext4Result<Option<TransactionId>> {
        let inner = lock(&self.inner);
        check_not_aborted(&inner)?;
        let Some(running) = inner.running.as_ref() else {
            return Ok(None);
        };
        if running.id() != transaction {
            return Err(Ext4Error::InvalidJournalTransaction);
        }
        let (active_handles, reserved_credits) = running.running_state()?;
        if active_handles != 0 || reserved_credits < self.max_reserved_credits {
            return Ok(None);
        }
        Ok(Some(running.id()))
    }

    /// Returns this transaction once all of its handles have closed.
    pub(crate) fn idle_running_transaction(
        &self,
        transaction: TransactionId,
    ) -> Ext4Result<Option<TransactionId>> {
        let inner = lock(&self.inner);
        check_not_aborted(&inner)?;
        let Some(running) = inner.running.as_ref() else {
            return Ok(None);
        };
        if running.id() != transaction {
            return Err(Ext4Error::InvalidJournalTransaction);
        }
        let (active_handles, _) = running.running_state()?;
        Ok((active_handles == 0).then_some(running.id()))
    }

    /// Returns the current running transaction, if one exists.
    pub(crate) fn running_transaction(&self) -> Ext4Result<Option<TransactionId>> {
        let inner = lock(&self.inner);
        check_not_aborted(&inner)?;
        Ok(inner.running.as_ref().map(|running| running.id()))
    }

    /// Returns whether this journal has been aborted.
    #[cfg(test)]
    pub fn is_aborted(&self) -> bool {
        lock(&self.inner).aborted.is_some()
    }

    /// Aborts the journal. After this, new handles and metadata writes fail.
    ///
    /// JBD2 abort is not a cross-operation memory rollback. The running
    /// transaction remains attached so existing handles can stop and release
    /// their reservations, but it can no longer accept updates or commit.
    pub fn abort(&self, error: Ext4Error) {
        let mut inner = lock(&self.inner);
        inner.aborted.get_or_insert(error);
        drop(inner);
        self.notify_checkpoint_waiters();
    }

    /// Freezes the running transaction into its committing phase.
    ///
    /// Persistence is performed separately while the same object remains in
    /// this phase. The transition refuses to run while callers hold handles.
    pub fn force_commit(&self, transaction: TransactionId) -> Ext4Result<Arc<RuntimeTransaction>> {
        let mut inner = lock(&self.inner);
        check_not_aborted(&inner)?;

        if let Some(commit) = inner
            .committing
            .as_ref()
            .filter(|commit| commit.id() == transaction)
        {
            return Ok(commit.clone());
        }
        if let Some(checkpoint) = inner
            .checkpoint_transactions
            .iter()
            .find(|checkpoint| checkpoint.id() == transaction)
        {
            return Ok(checkpoint.clone());
        }
        if inner.committing.is_some() {
            return Err(Ext4Error::JournalBusy);
        }

        if inner
            .running
            .as_ref()
            .is_some_and(|running| running.id() == transaction)
        {
            let Some(running) = inner.running.as_ref() else {
                return Err(Ext4Error::InvalidJournalTransaction);
            };
            running.freeze_for_commit()?;
            let commit = inner
                .running
                .take()
                .ok_or(Ext4Error::InvalidJournalTransaction)?;
            inner.committing = Some(commit.clone());
            drop(inner);
            self.notify_checkpoint_waiters();
            return Ok(commit);
        }

        Err(Ext4Error::InvalidJournalTransaction)
    }

    /// Confirms that this exact transaction object is owned by this journal's
    /// committing phase.
    pub(crate) fn validate_committing(&self, commit: &Arc<RuntimeTransaction>) -> Ext4Result<()> {
        let inner = lock(&self.inner);
        check_not_aborted(&inner)?;
        if inner
            .committing
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, commit))
        {
            Ok(())
        } else {
            Err(Ext4Error::InvalidJournalTransaction)
        }
    }

    /// Moves the committing transaction into the checkpoint queue.
    pub(crate) fn start_checkpoint(
        &self,
        commit: &Arc<RuntimeTransaction>,
        persisted: JournalPersistedCommit,
    ) -> Ext4Result<()> {
        let mut inner = lock(&self.inner);
        check_not_aborted(&inner)?;
        if inner
            .checkpoint_transactions
            .iter()
            .any(|checkpoint| Arc::ptr_eq(checkpoint, commit))
        {
            return Ok(());
        }
        let committing = inner
            .committing
            .take()
            .ok_or(Ext4Error::InvalidJournalTransaction)?;
        if !Arc::ptr_eq(&committing, commit) {
            inner.committing = Some(committing);
            return Err(Ext4Error::InvalidJournalTransaction);
        }
        if let Err(error) = committing.start_checkpoint(Some(persisted)) {
            inner.committing = Some(committing);
            return Err(error);
        }
        inner.checkpoint_transactions.push_back(committing);
        drop(inner);
        self.notify_checkpoint_waiters();
        Ok(())
    }

    /// Returns the oldest transaction awaiting home-block checkpoint.
    pub(crate) fn checkpoint_transaction(
        &self,
    ) -> Ext4Result<Option<(Arc<RuntimeTransaction>, JournalPersistedCommit)>> {
        let inner = lock(&self.inner);
        check_not_aborted(&inner)?;
        inner
            .checkpoint_transactions
            .front()
            .map(|checkpoint| {
                checkpoint
                    .persisted_commit()?
                    .map(|persisted| (checkpoint.clone(), persisted))
                    .ok_or(Ext4Error::InvalidJournalTransaction)
            })
            .transpose()
    }

    /// Confirms that this exact transaction and persistence evidence form the
    /// oldest checkpoint owned by this journal.
    pub(crate) fn validate_checkpoint(
        &self,
        commit: &Arc<RuntimeTransaction>,
        persisted: &JournalPersistedCommit,
    ) -> Ext4Result<()> {
        let inner = lock(&self.inner);
        check_not_aborted(&inner)?;
        let checkpoint = inner
            .checkpoint_transactions
            .front()
            .ok_or(Ext4Error::InvalidJournalTransaction)?;
        if !Arc::ptr_eq(checkpoint, commit) || checkpoint.persisted_commit()? != Some(*persisted) {
            return Err(Ext4Error::InvalidJournalTransaction);
        }
        Ok(())
    }

    /// Returns whether at least one transaction awaits checkpoint.
    pub(crate) fn has_checkpoint_transactions(&self) -> bool {
        !lock(&self.inner).checkpoint_transactions.is_empty()
    }

    /// Returns the persisted state following the newest checkpoint candidate.
    pub(crate) fn newest_persisted_commit(&self) -> Ext4Result<Option<JournalPersistedCommit>> {
        lock(&self.inner)
            .checkpoint_transactions
            .back()
            .map(|checkpoint| checkpoint.persisted_commit())
            .transpose()
            .map(Option::flatten)
    }

    /// Returns the persisted state that follows the active checkpoint.
    pub(crate) fn next_checkpoint_persisted_commit(
        &self,
    ) -> Ext4Result<Option<JournalPersistedCommit>> {
        lock(&self.inner)
            .checkpoint_transactions
            .get(1)
            .map(|checkpoint| checkpoint.persisted_commit())
            .transpose()
            .map(Option::flatten)
    }

    /// Marks the oldest checkpoint transaction complete.
    pub fn finish_checkpoint(
        &self,
        commit: &Arc<RuntimeTransaction>,
    ) -> Ext4Result<JournalCheckpoint> {
        let mut inner = lock(&self.inner);
        check_not_aborted(&inner)?;
        if is_checkpointed(&inner, commit.id()) {
            return Ok(JournalCheckpoint {
                transaction: commit.id(),
            });
        }
        let checkpoint = inner
            .checkpoint_transactions
            .front()
            .ok_or(Ext4Error::InvalidJournalTransaction)?;
        if !Arc::ptr_eq(checkpoint, commit) {
            return Err(Ext4Error::InvalidJournalTransaction);
        }
        if commit.id() != next_transaction(inner.checkpointed_through) {
            return Err(Ext4Error::InvalidJournalTransaction);
        }
        commit.finish_checkpoint()?;
        inner.checkpoint_transactions.pop_front();
        inner.checkpointed_through = commit.id();
        drop(inner);
        self.notify_checkpoint_waiters();
        Ok(JournalCheckpoint {
            transaction: commit.id(),
        })
    }

    #[cfg(test)]
    pub(crate) fn start_checkpoint_for_test(
        &self,
        commit: &Arc<RuntimeTransaction>,
    ) -> Ext4Result<()> {
        let mut inner = lock(&self.inner);
        let committing = inner
            .committing
            .take()
            .ok_or(Ext4Error::InvalidJournalTransaction)?;
        if !Arc::ptr_eq(&committing, commit) {
            inner.committing = Some(committing);
            return Err(Ext4Error::InvalidJournalTransaction);
        }
        if let Err(error) = committing.start_checkpoint(None) {
            inner.committing = Some(committing);
            return Err(error);
        }
        inner.checkpoint_transactions.push_back(committing);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn finish_checkpoint_for_test(
        &self,
        commit: &Arc<RuntimeTransaction>,
    ) -> Ext4Result<JournalCheckpoint> {
        {
            let inner = lock(&self.inner);
            if inner
                .committing
                .as_ref()
                .is_some_and(|committing| Arc::ptr_eq(committing, commit))
            {
                drop(inner);
                self.start_checkpoint_for_test(commit)?;
            }
        }
        self.finish_checkpoint(commit)
    }

    /// Waits for a committed transaction's checkpoint state.
    #[cfg(all(test, not(target_os = "none")))]
    pub fn wait_checkpoint(&self, transaction: TransactionId) -> Ext4Result<JournalCheckpoint> {
        let mut inner = lock(&self.inner);
        loop {
            check_not_aborted(&inner)?;
            if is_checkpointed(&inner, transaction) {
                return Ok(JournalCheckpoint { transaction });
            }
            if is_checkpoint_pending(&inner, transaction) {
                inner = self
                    .checkpoint_condition
                    .wait(inner)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                continue;
            }
            return Err(Ext4Error::InvalidJournalTransaction);
        }
    }

    /// Waits for a committed transaction's checkpoint state.
    #[cfg(all(test, target_os = "none"))]
    pub fn wait_checkpoint(&self, transaction: TransactionId) -> Ext4Result<JournalCheckpoint> {
        self.checkpoint_waiters.wait_until(|| {
            let inner = lock(&self.inner);
            inner.aborted.is_some()
                || is_checkpointed(&inner, transaction)
                || !is_checkpoint_pending(&inner, transaction)
        });
        let inner = lock(&self.inner);
        check_not_aborted(&inner)?;
        if is_checkpointed(&inner, transaction) {
            Ok(JournalCheckpoint { transaction })
        } else {
            Err(Ext4Error::InvalidJournalTransaction)
        }
    }

    fn finish_handle(&self, transaction: TransactionId, unused_credits: u32) -> Ext4Result<()> {
        let inner = lock(&self.inner);
        let running = inner
            .running
            .as_ref()
            .ok_or(Ext4Error::InvalidJournalTransaction)?;
        if running.id() != transaction {
            return Err(Ext4Error::InvalidJournalTransaction);
        }
        running.with_running(|running| running.finish_handle(unused_credits))
    }

    fn with_running<R>(
        &self,
        transaction: TransactionId,
        f: impl FnOnce(&mut RunningTransactionState) -> Ext4Result<R>,
    ) -> Ext4Result<R> {
        let inner = lock(&self.inner);
        check_not_aborted(&inner)?;
        let running = inner.running.as_ref().ok_or(Ext4Error::JournalBusy)?;
        if running.id() != transaction {
            return Err(Ext4Error::JournalBusy);
        }
        running.with_running(f)
    }

    #[cfg(target_os = "none")]
    fn notify_checkpoint_waiters(&self) {
        self.checkpoint_waiters.notify_all(false);
    }

    #[cfg(not(target_os = "none"))]
    fn notify_checkpoint_waiters(&self) {
        self.checkpoint_condition.notify_all();
    }
}

fn check_not_aborted(inner: &JournalInner) -> Ext4Result<()> {
    if inner.aborted.is_some() {
        Err(Ext4Error::JournalAborted)
    } else {
        Ok(())
    }
}

const fn previous_transaction(transaction: TransactionId) -> TransactionId {
    TransactionId::new(transaction.get().wrapping_sub(1))
}

const fn next_transaction(transaction: TransactionId) -> TransactionId {
    TransactionId::new(transaction.get().wrapping_add(1))
}

fn is_checkpointed(inner: &JournalInner, transaction: TransactionId) -> bool {
    transaction_is_same_or_older(transaction, inner.checkpointed_through)
}

/// JBD2 transaction ids wrap at `u32::MAX`; live transactions are required to
/// stay within half the sequence space, matching Linux's signed-delta rule.
const fn transaction_is_same_or_older(
    transaction: TransactionId,
    reference: TransactionId,
) -> bool {
    reference.get().wrapping_sub(transaction.get()) as i32 >= 0
}

#[cfg(test)]
fn is_checkpoint_pending(inner: &JournalInner, transaction: TransactionId) -> bool {
    inner
        .running
        .as_ref()
        .is_some_and(|running| running.id() == transaction)
        || inner
            .committing
            .as_ref()
            .is_some_and(|commit| commit.id() == transaction)
        || inner
            .checkpoint_transactions
            .iter()
            .any(|checkpoint| checkpoint.id() == transaction)
}

/// A live reference to the running transaction plus its remaining credits.
pub struct JournalHandle<'a> {
    journal: &'a JournalTransactions,
    id: TransactionId,
    remaining_credits: u32,
    has_updates: bool,
    closed: bool,
}

impl<'a> JournalHandle<'a> {
    /// Returns the transaction id used by metadata buffers.
    pub const fn id(&self) -> TransactionId {
        self.id
    }

    /// Returns whether this handle has joined metadata or revoke state.
    pub(crate) const fn has_updates(&self) -> bool {
        self.has_updates
    }

    /// Returns remaining credits local to this handle.
    pub const fn remaining_credits(&self) -> u32 {
        self.remaining_credits
    }

    /// Reserves additional credits without exceeding the running transaction's
    /// journal-space limit.
    ///
    /// # Errors
    ///
    /// Returns [`Ext4Error::InsufficientJournalCredits`] when the extended
    /// reservation would exceed the mounted journal's transaction limit.
    pub(crate) fn reserve_more(&mut self, credits: JournalCredits) -> Ext4Result<()> {
        self.journal.with_running(self.id, |running| {
            let projected_credits = running
                .reserved_credits
                .checked_add(credits.get())
                .ok_or(Ext4Error::Overflow)?;
            if projected_credits > self.journal.max_reserved_credits {
                return Err(Ext4Error::InsufficientJournalCredits);
            }
            running.reserve(credits)?;
            self.remaining_credits = self
                .remaining_credits
                .checked_add(credits.get())
                .ok_or(Ext4Error::Overflow)?;
            Ok(())
        })
    }

    pub(crate) fn consume_metadata_credit(&mut self, block: FilesystemBlock) -> Ext4Result<()> {
        self.journal.with_running(self.id, |running| {
            if running.metadata_blocks.contains(&block) {
                return Ok(());
            }
            let reused_revoke_credit = running.remove_revoke_block(block);
            if !reused_revoke_credit {
                if self.remaining_credits < JournalCredits::one().get() {
                    return Err(Ext4Error::InsufficientJournalCredits);
                }
                running.consume(JournalCredits::one())?;
                self.remaining_credits -= JournalCredits::one().get();
            }
            running.metadata_blocks.insert(block);
            Ok(())
        })?;
        // Access to an already-owned block can still publish new bytes for this
        // handle. Track access rather than transaction-wide set insertion so a
        // later ordinary error cannot leave those bytes commit-eligible.
        self.has_updates = true;
        Ok(())
    }

    pub(crate) fn ensure_revoke_metadata_credit(&self, block: FilesystemBlock) -> Ext4Result<()> {
        self.journal.with_running(self.id, |running| {
            if running.revoked_blocks.contains(&block)
                || running.metadata_blocks.contains(&block)
                || self.remaining_credits >= JournalCredits::one().get()
            {
                Ok(())
            } else {
                Err(Ext4Error::InsufficientJournalCredits)
            }
        })
    }

    pub(crate) fn revoke_metadata_block(&mut self, block: FilesystemBlock) -> Ext4Result<()> {
        self.journal.with_running(self.id, |running| {
            if running.revoked_blocks.contains(&block) {
                return Ok(());
            }
            let reused_metadata_credit = running.metadata_blocks.remove(&block);
            if !reused_metadata_credit {
                if self.remaining_credits < JournalCredits::one().get() {
                    return Err(Ext4Error::InsufficientJournalCredits);
                }
                running.consume(JournalCredits::one())?;
                self.remaining_credits -= JournalCredits::one().get();
            }
            running.revoked_blocks.insert(block);
            Ok(())
        })?;
        self.has_updates = true;
        Ok(())
    }

    pub(crate) fn forget_metadata_block_without_revoke(
        &mut self,
        block: FilesystemBlock,
    ) -> Ext4Result<()> {
        let removed_metadata = self.journal.with_running(self.id, |running| {
            let removed_metadata = running.metadata_blocks.remove(&block);
            if removed_metadata {
                running.refund(JournalCredits::one())?;
                self.remaining_credits = self.remaining_credits.saturating_add(1);
            }
            Ok(removed_metadata)
        })?;
        self.has_updates |= removed_metadata;
        Ok(())
    }

    /// Stops this handle and returns unused credits to its transaction.
    pub(crate) fn stop(mut self) -> Ext4Result<()> {
        self.finish()
    }

    fn finish(&mut self) -> Ext4Result<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        self.journal.finish_handle(self.id, self.remaining_credits)
    }
}

impl Drop for JournalHandle<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.finish() {
            self.journal.abort(error);
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(not(target_os = "none"))]
    use std::{sync::mpsc, thread, time::Duration};

    use super::*;
    use crate::FilesystemBlock;

    #[test]
    fn handles_share_running_transaction_and_consume_credits() {
        let journal = JournalTransactions::new(TransactionId::new(3));
        let mut first = journal.begin(JournalCredits::new(2)).unwrap();
        let second = journal.begin(JournalCredits::new(1)).unwrap();

        assert_eq!(first.id(), TransactionId::new(3));
        assert_eq!(second.id(), TransactionId::new(3));
        first
            .consume_metadata_credit(FilesystemBlock::new(10))
            .unwrap();
        first
            .consume_metadata_credit(FilesystemBlock::new(11))
            .unwrap();
        assert_eq!(first.remaining_credits(), 0);
        assert_eq!(
            first.consume_metadata_credit(FilesystemBlock::new(12)),
            Err(Ext4Error::InsufficientJournalCredits)
        );
    }

    #[test]
    fn reserve_more_extends_handle_credit_budget() {
        let journal = JournalTransactions::new_with_credit_limit(TransactionId::new(8), 3);
        let mut handle = journal.begin(JournalCredits::new(1)).unwrap();

        handle
            .consume_metadata_credit(FilesystemBlock::new(1))
            .unwrap();
        handle.reserve_more(JournalCredits::new(2)).unwrap();
        assert_eq!(handle.remaining_credits(), 2);
        handle
            .consume_metadata_credit(FilesystemBlock::new(2))
            .unwrap();
    }

    #[test]
    fn reserve_more_rejects_transaction_limit_overflow() {
        let journal = JournalTransactions::new_with_credit_limit(TransactionId::new(8), 3);
        let mut handle = journal.begin(JournalCredits::new(2)).unwrap();

        assert_eq!(
            handle.reserve_more(JournalCredits::new(2)),
            Err(Ext4Error::InsufficientJournalCredits)
        );
        assert_eq!(handle.remaining_credits(), 2);
    }

    #[test]
    fn repeated_metadata_block_consumes_one_credit() {
        let journal = JournalTransactions::new(TransactionId::new(10));
        let mut handle = journal.begin(JournalCredits::new(1)).unwrap();
        let transaction = handle.id();

        handle
            .consume_metadata_credit(FilesystemBlock::new(5))
            .unwrap();
        handle
            .consume_metadata_credit(FilesystemBlock::new(5))
            .unwrap();
        assert_eq!(handle.remaining_credits(), 0);
        drop(handle);

        let commit = journal.force_commit(transaction).unwrap();
        assert_eq!(commit.used_credits().unwrap(), 1);
        assert_eq!(
            commit.metadata_blocks().unwrap().as_ref(),
            &[FilesystemBlock::new(5)]
        );
    }

    #[test]
    fn shared_metadata_access_marks_each_handle_without_duplicate_membership() {
        let journal = JournalTransactions::new(TransactionId::new(11));
        let mut first = journal.begin(JournalCredits::new(1)).unwrap();
        let transaction = first.id();
        let block = FilesystemBlock::new(5);

        first.consume_metadata_credit(block).unwrap();
        let mut second = journal.begin(JournalCredits::new(1)).unwrap();
        second.consume_metadata_credit(block).unwrap();
        assert!(first.has_updates());
        assert!(second.has_updates());
        drop(first);
        drop(second);

        let commit = journal.force_commit(transaction).unwrap();
        assert_eq!(commit.used_credits().unwrap(), 1);
        assert_eq!(commit.metadata_blocks().unwrap().as_ref(), &[block]);
    }

    #[test]
    fn revoke_metadata_block_consumes_one_credit_once() {
        let journal = JournalTransactions::new(TransactionId::new(14));
        let mut handle = journal.begin(JournalCredits::new(1)).unwrap();
        let transaction = handle.id();
        let block = FilesystemBlock::new(9);

        handle.revoke_metadata_block(block).unwrap();
        handle.revoke_metadata_block(block).unwrap();
        assert_eq!(handle.remaining_credits(), 0);
        drop(handle);

        let commit = journal.force_commit(transaction).unwrap();
        assert_eq!(commit.used_credits().unwrap(), 1);
        assert!(commit.metadata_blocks().unwrap().as_ref().is_empty());
        assert_eq!(commit.revoked_blocks().unwrap().as_ref(), &[block]);
    }

    #[test]
    fn revoke_replaces_metadata_update_in_same_transaction() {
        let journal = JournalTransactions::new(TransactionId::new(15));
        let mut handle = journal.begin(JournalCredits::new(1)).unwrap();
        let transaction = handle.id();
        let block = FilesystemBlock::new(10);

        handle.consume_metadata_credit(block).unwrap();
        handle.revoke_metadata_block(block).unwrap();
        assert_eq!(handle.remaining_credits(), 0);
        drop(handle);

        let commit = journal.force_commit(transaction).unwrap();
        assert_eq!(commit.used_credits().unwrap(), 1);
        assert!(commit.metadata_blocks().unwrap().as_ref().is_empty());
        assert_eq!(commit.revoked_blocks().unwrap().as_ref(), &[block]);
    }

    #[test]
    fn metadata_update_cancels_revoke_in_same_transaction() {
        let journal = JournalTransactions::new(TransactionId::new(16));
        let mut handle = journal.begin(JournalCredits::new(1)).unwrap();
        let transaction = handle.id();
        let block = FilesystemBlock::new(11);

        handle.revoke_metadata_block(block).unwrap();
        handle.consume_metadata_credit(block).unwrap();
        assert_eq!(handle.remaining_credits(), 0);
        drop(handle);

        let commit = journal.force_commit(transaction).unwrap();
        assert_eq!(commit.used_credits().unwrap(), 1);
        assert_eq!(commit.metadata_blocks().unwrap().as_ref(), &[block]);
        assert!(commit.revoked_blocks().unwrap().as_ref().is_empty());
    }

    #[test]
    fn abort_blocks_new_and_existing_handles() {
        let journal = JournalTransactions::new(TransactionId::new(1));
        let mut handle = journal.begin(JournalCredits::new(1)).unwrap();

        journal.abort(Ext4Error::Device(block::DriverError::Io));

        assert!(journal.is_aborted());
        assert_eq!(
            journal.begin(JournalCredits::new(1)).map(|_| ()),
            Err(Ext4Error::JournalAborted)
        );
        assert_eq!(
            handle.consume_metadata_credit(FilesystemBlock::new(1)),
            Err(Ext4Error::JournalAborted)
        );
    }

    #[test]
    fn commit_requires_all_handles_to_close() {
        let journal = JournalTransactions::new(TransactionId::new(1));
        let handle = journal.begin(JournalCredits::new(1)).unwrap();
        let transaction = handle.id();

        assert!(matches!(
            journal.force_commit(transaction),
            Err(Ext4Error::JournalBusy)
        ));
        drop(handle);

        let commit = journal.force_commit(transaction).unwrap();
        let checkpoint = journal.finish_checkpoint_for_test(&commit).unwrap();
        assert_eq!(checkpoint.transaction(), transaction);
    }

    #[cfg(not(target_os = "none"))]
    #[test]
    fn wait_checkpoint_blocks_until_checkpoint_finishes() {
        let journal = Arc::new(JournalTransactions::new(TransactionId::new(17)));
        let handle = journal.begin(JournalCredits::new(1)).unwrap();
        let transaction = handle.id();
        drop(handle);
        let commit = journal.force_commit(transaction).unwrap();
        let (ready_sender, ready_receiver) = mpsc::channel();
        let (done_sender, done_receiver) = mpsc::channel();
        let waiter_journal = journal.clone();

        thread::spawn(move || {
            ready_sender.send(()).unwrap();
            let checkpoint = waiter_journal.wait_checkpoint(transaction).unwrap();
            done_sender.send(checkpoint.transaction()).unwrap();
        });

        ready_receiver.recv().unwrap();
        assert!(
            done_receiver
                .recv_timeout(Duration::from_millis(20))
                .is_err()
        );
        journal.finish_checkpoint_for_test(&commit).unwrap();
        assert_eq!(
            done_receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
            transaction
        );
    }

    #[cfg(not(target_os = "none"))]
    #[test]
    fn wait_checkpoint_wakes_when_journal_aborts() {
        let journal = Arc::new(JournalTransactions::new(TransactionId::new(18)));
        let handle = journal.begin(JournalCredits::new(1)).unwrap();
        let transaction = handle.id();
        let (ready_sender, ready_receiver) = mpsc::channel();
        let (done_sender, done_receiver) = mpsc::channel();
        let waiter_journal = journal.clone();

        thread::spawn(move || {
            ready_sender.send(()).unwrap();
            done_sender
                .send(waiter_journal.wait_checkpoint(transaction))
                .unwrap();
        });

        ready_receiver.recv().unwrap();
        assert!(
            done_receiver
                .recv_timeout(Duration::from_millis(20))
                .is_err()
        );
        journal.abort(Ext4Error::Device(block::DriverError::Io));
        assert_eq!(
            done_receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
            Err(Ext4Error::JournalAborted)
        );
        drop(handle);
    }

    #[test]
    fn handle_stop_returns_unused_credits() {
        let journal = JournalTransactions::new(TransactionId::new(5));
        let mut handle = journal.begin(JournalCredits::new(3)).unwrap();
        let transaction = handle.id();
        handle
            .consume_metadata_credit(FilesystemBlock::new(12))
            .unwrap();
        handle.stop().unwrap();

        let commit = journal.force_commit(transaction).unwrap();
        assert_eq!(commit.reserved_credits().unwrap(), 1);
        assert_eq!(commit.used_credits().unwrap(), 1);
    }

    #[test]
    fn one_handle_can_finish_while_another_handle_is_open() {
        let journal = JournalTransactions::new(TransactionId::new(6));
        let mut first = journal.begin(JournalCredits::new(1)).unwrap();
        let transaction = first.id();
        first
            .consume_metadata_credit(FilesystemBlock::new(1))
            .unwrap();
        let second = journal.begin(JournalCredits::new(1)).unwrap();

        drop(first);
        assert_eq!(
            journal.force_commit(transaction).map(|_| ()),
            Err(Ext4Error::JournalBusy)
        );
        drop(second);
        assert_eq!(journal.force_commit(transaction).unwrap().id(), transaction);
    }

    #[test]
    fn commit_records_metadata_blocks_and_used_credits() {
        let journal = JournalTransactions::new(TransactionId::new(9));
        let mut handle = journal.begin(JournalCredits::new(3)).unwrap();
        let transaction = handle.id();

        handle
            .consume_metadata_credit(FilesystemBlock::new(7))
            .unwrap();
        handle
            .consume_metadata_credit(FilesystemBlock::new(3))
            .unwrap();
        drop(handle);

        let commit = journal.force_commit(transaction).unwrap();
        assert_eq!(commit.id(), transaction);
        assert_eq!(commit.reserved_credits().unwrap(), 2);
        assert_eq!(commit.used_credits().unwrap(), 2);
        assert_eq!(
            commit.metadata_blocks().unwrap().as_ref(),
            &[FilesystemBlock::new(3), FilesystemBlock::new(7)]
        );
    }

    #[test]
    fn force_commit_returns_the_same_committing_transaction() {
        let journal = JournalTransactions::new(TransactionId::new(13));
        let mut handle = journal.begin(JournalCredits::new(1)).unwrap();
        let transaction = handle.id();
        handle
            .consume_metadata_credit(FilesystemBlock::new(1))
            .unwrap();
        drop(handle);

        let first = journal.force_commit(transaction).unwrap();
        let second = journal.force_commit(transaction).unwrap();

        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn runtime_transaction_is_one_object_across_all_phases() {
        let journal = JournalTransactions::new(TransactionId::new(19));
        let handle = journal.begin(JournalCredits::new(1)).unwrap();
        let transaction = handle.id();
        handle.stop().unwrap();

        let commit = journal.force_commit(transaction).unwrap();
        assert!(matches!(
            &*lock(&commit.phase),
            TransactionPhase::Committing(_)
        ));

        journal.start_checkpoint_for_test(&commit).unwrap();
        {
            let inner = lock(&journal.inner);
            assert!(Arc::ptr_eq(
                inner.checkpoint_transactions.front().unwrap(),
                &commit
            ));
        }
        assert!(matches!(
            &*lock(&commit.phase),
            TransactionPhase::Checkpoint { .. }
        ));

        journal.finish_checkpoint(&commit).unwrap();
        assert!(matches!(&*lock(&commit.phase), TransactionPhase::Finished));
    }

    #[test]
    fn committing_transaction_rejects_a_foreign_journal_object() {
        let first = JournalTransactions::new(TransactionId::new(22));
        let second = JournalTransactions::new(TransactionId::new(22));
        let first_handle = first.begin(JournalCredits::new(1)).unwrap();
        let second_handle = second.begin(JournalCredits::new(1)).unwrap();
        let transaction = first_handle.id();
        first_handle.stop().unwrap();
        second_handle.stop().unwrap();
        let first_commit = first.force_commit(transaction).unwrap();
        let second_commit = second.force_commit(transaction).unwrap();

        first.validate_committing(&first_commit).unwrap();
        assert_eq!(
            first.validate_committing(&second_commit),
            Err(Ext4Error::InvalidJournalTransaction)
        );
    }

    #[test]
    fn abort_preserves_running_transaction_accounting_until_handles_stop() {
        let journal = JournalTransactions::new(TransactionId::new(20));
        let handle = journal.begin(JournalCredits::new(2)).unwrap();
        let transaction = handle.id();

        journal.abort(Ext4Error::Device(block::DriverError::Io));
        assert!(matches!(
            journal.force_commit(transaction),
            Err(Ext4Error::JournalAborted)
        ));
        handle.stop().unwrap();
        assert_eq!(
            journal.begin(JournalCredits::new(1)).map(|_| ()),
            Err(Ext4Error::JournalAborted)
        );
    }

    #[test]
    fn empty_handle_can_close_and_commit_without_metadata() {
        let journal = JournalTransactions::new(TransactionId::new(21));
        let handle = journal.begin(JournalCredits::new(1)).unwrap();
        let transaction = handle.id();
        drop(handle);

        let commit = journal.force_commit(transaction).unwrap();
        assert_eq!(commit.id(), transaction);
        assert_eq!(commit.used_credits().unwrap(), 0);
        assert!(commit.metadata_blocks().unwrap().as_ref().is_empty());
    }

    #[test]
    fn running_transaction_trigger_accounts_for_outstanding_credits() {
        let journal = JournalTransactions::new_with_credit_limit(TransactionId::new(32), 4);
        let mut first = journal.begin(JournalCredits::new(2)).unwrap();
        let transaction = first.id();
        first
            .consume_metadata_credit(FilesystemBlock::new(1))
            .unwrap();
        first
            .consume_metadata_credit(FilesystemBlock::new(2))
            .unwrap();
        drop(first);

        assert_eq!(journal.running_transaction().unwrap(), Some(transaction));
        assert_eq!(
            journal
                .transaction_to_commit_before_reservation(JournalCredits::new(1))
                .unwrap(),
            None
        );
        assert_eq!(
            journal
                .transaction_to_commit_before_reservation(JournalCredits::new(2))
                .unwrap(),
            Some(transaction)
        );
    }

    #[test]
    fn admission_rejects_one_handle_that_exceeds_limit() {
        let journal = JournalTransactions::new_with_credit_limit(TransactionId::new(33), 4);

        assert_eq!(
            journal
                .transaction_to_commit_before_reservation(JournalCredits::new(4))
                .unwrap(),
            None
        );
        assert_eq!(
            journal.transaction_to_commit_before_reservation(JournalCredits::new(5)),
            Err(Ext4Error::InsufficientJournalCredits)
        );
        assert_eq!(
            journal.begin(JournalCredits::new(5)).map(|_| ()),
            Err(Ext4Error::InsufficientJournalCredits)
        );
        assert_eq!(journal.running_transaction().unwrap(), None);
    }

    #[test]
    fn admission_allows_active_handles_below_limit_and_waits_at_limit() {
        let journal = JournalTransactions::new_with_credit_limit(TransactionId::new(34), 4);
        let mut handle = journal.begin(JournalCredits::new(2)).unwrap();
        let transaction = handle.id();
        handle
            .consume_metadata_credit(FilesystemBlock::new(1))
            .unwrap();
        handle
            .consume_metadata_credit(FilesystemBlock::new(2))
            .unwrap();

        assert_eq!(
            journal
                .transaction_to_commit_before_reservation(JournalCredits::new(1))
                .unwrap(),
            None
        );
        assert_eq!(
            journal.transaction_to_commit_before_reservation(JournalCredits::new(2)),
            Err(Ext4Error::JournalBusy)
        );

        drop(handle);
        assert_eq!(
            journal
                .transaction_to_commit_before_reservation(JournalCredits::new(2))
                .unwrap(),
            Some(transaction)
        );
    }

    #[test]
    fn completion_requests_commit_only_after_the_last_handle_closes() {
        let journal = JournalTransactions::new_with_credit_limit(TransactionId::new(35), 2);
        let mut first = journal.begin(JournalCredits::new(2)).unwrap();
        let transaction = first.id();
        first
            .consume_metadata_credit(FilesystemBlock::new(1))
            .unwrap();
        first
            .consume_metadata_credit(FilesystemBlock::new(2))
            .unwrap();
        let second = journal.begin(JournalCredits::new(1)).unwrap();

        drop(first);
        assert_eq!(
            journal
                .transaction_to_commit_after_handle(transaction)
                .unwrap(),
            None
        );
        drop(second);
        assert_eq!(
            journal
                .transaction_to_commit_after_handle(transaction)
                .unwrap(),
            Some(transaction)
        );
    }

    #[test]
    fn completed_checkpoints_release_committed_transactions() {
        let journal = JournalTransactions::new(TransactionId::new(40));
        let first_handle = journal.begin(JournalCredits::new(1)).unwrap();
        let first_transaction = first_handle.id();
        drop(first_handle);
        let first = journal.force_commit(first_transaction).unwrap();
        journal.start_checkpoint_for_test(&first).unwrap();
        let second_handle = journal.begin(JournalCredits::new(1)).unwrap();
        let second_transaction = second_handle.id();
        drop(second_handle);
        let second = journal.force_commit(second_transaction).unwrap();

        journal.finish_checkpoint_for_test(&first).unwrap();
        journal.finish_checkpoint_for_test(&second).unwrap();

        let inner = lock(&journal.inner);
        assert!(inner.committing.is_none());
        assert!(inner.checkpoint_transactions.is_empty());
        assert_eq!(inner.checkpointed_through, second_transaction);
        drop(inner);
        assert_eq!(
            journal
                .wait_checkpoint(first_transaction)
                .unwrap()
                .transaction(),
            first_transaction
        );
    }

    #[test]
    fn checkpoint_rejects_out_of_order_transaction() {
        let journal = JournalTransactions::new(TransactionId::new(50));
        let first_handle = journal.begin(JournalCredits::new(1)).unwrap();
        let first_transaction = first_handle.id();
        drop(first_handle);
        let first = journal.force_commit(first_transaction).unwrap();
        journal.start_checkpoint_for_test(&first).unwrap();
        let second_handle = journal.begin(JournalCredits::new(1)).unwrap();
        let second_transaction = second_handle.id();
        drop(second_handle);
        let second = journal.force_commit(second_transaction).unwrap();

        assert_eq!(
            journal.finish_checkpoint_for_test(&second),
            Err(Ext4Error::InvalidJournalTransaction)
        );
        journal.finish_checkpoint_for_test(&first).unwrap();
        journal.finish_checkpoint_for_test(&second).unwrap();

        let inner = lock(&journal.inner);
        assert_eq!(inner.checkpointed_through, second_transaction);
    }

    #[test]
    fn checkpoint_watermark_advances_across_transaction_wrap() {
        let journal = JournalTransactions::new(TransactionId::new(u32::MAX));
        let last_handle = journal.begin(JournalCredits::new(1)).unwrap();
        let last_transaction = last_handle.id();
        drop(last_handle);
        let last = journal.force_commit(last_transaction).unwrap();
        journal.start_checkpoint_for_test(&last).unwrap();
        let wrapped_handle = journal.begin(JournalCredits::new(1)).unwrap();
        let wrapped_transaction = wrapped_handle.id();
        drop(wrapped_handle);
        let wrapped = journal.force_commit(wrapped_transaction).unwrap();

        journal.finish_checkpoint_for_test(&last).unwrap();
        journal.finish_checkpoint_for_test(&wrapped).unwrap();

        let inner = lock(&journal.inner);
        assert_eq!(inner.checkpointed_through, TransactionId::new(0));
    }
}

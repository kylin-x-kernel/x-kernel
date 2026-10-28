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

use ksync::{Mutex, MutexGuard};
use ktask::WaitQueue;

use super::{
    JournalPersistedCommit, JournalSuperblock, TransactionId,
    commit::{journal_checkpoint_target_blocks, journal_checkpoint_target_reached},
};
use crate::{Ext4Error, Ext4Result, FilesystemBlock};

mod progress;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock()
}

/// Metadata journal credits reserved for one operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct JournalCredits(u32);

impl JournalCredits {
    /// Creates a credit count.
    pub(crate) const fn new(value: u32) -> Self {
        Self(value)
    }

    /// Returns the raw credit count.
    pub(crate) const fn get(self) -> u32 {
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
    ordered_generation: u64,
    flushed_generation: u64,
    metadata_blocks: BTreeSet<FilesystemBlock>,
    revoked_blocks: BTreeSet<FilesystemBlock>,
}

impl RunningTransactionState {
    fn new() -> Self {
        Self {
            active_handles: 0,
            reserved_credits: 0,
            used_credits: 0,
            ordered_generation: 0,
            flushed_generation: 0,
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
            has_unflushed_ordered_data: self.ordered_generation != self.flushed_generation,
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
    closing: Option<TransactionId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FrozenTransactionState {
    reserved_credits: u32,
    used_credits: u32,
    has_unflushed_ordered_data: bool,
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
pub(crate) struct RuntimeTransaction {
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
    pub(crate) const fn id(&self) -> TransactionId {
        self.id
    }

    /// Returns the credits reserved by handles in this transaction.
    #[cfg(unittest)]
    pub(crate) fn reserved_credits(&self) -> Ext4Result<u32> {
        Ok(self.frozen_state()?.reserved_credits)
    }

    /// Returns credits consumed for metadata buffer write access.
    #[cfg(unittest)]
    pub(crate) fn used_credits(&self) -> Ext4Result<u32> {
        Ok(self.frozen_state()?.used_credits)
    }

    /// Returns metadata blocks touched by this transaction.
    pub(crate) fn metadata_blocks(&self) -> Ext4Result<Arc<[FilesystemBlock]>> {
        Ok(self.frozen_state()?.metadata_blocks)
    }

    /// Returns filesystem blocks revoked by this transaction.
    pub(crate) fn revoked_blocks(&self) -> Ext4Result<Arc<[FilesystemBlock]>> {
        Ok(self.frozen_state()?.revoked_blocks)
    }

    /// Returns whether committing metadata still depends on volatile data writes.
    pub(crate) fn has_unflushed_ordered_data(&self) -> Ext4Result<bool> {
        let phase = lock(&self.phase);
        let TransactionPhase::Committing(frozen) = &*phase else {
            return Err(Ext4Error::InvalidJournalTransaction);
        };
        Ok(frozen.has_unflushed_ordered_data)
    }

    /// Flushes pending ordered data before acknowledging its durability.
    ///
    /// The mounted journal serializes persistence. This transaction is frozen,
    /// so it accepts no new data dependencies. No phase lock is held during I/O.
    pub(crate) fn flush_ordered_data(
        &self,
        barrier_fn: impl FnOnce() -> Ext4Result<()>,
    ) -> Ext4Result<()> {
        if !self.has_unflushed_ordered_data()? {
            return Ok(());
        }
        barrier_fn()?;
        let mut phase = lock(&self.phase);
        let TransactionPhase::Committing(frozen) = &mut *phase else {
            return Err(Ext4Error::InvalidJournalTransaction);
        };
        frozen.has_unflushed_ordered_data = false;
        Ok(())
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
            TransactionPhase::Committing(frozen) if !frozen.has_unflushed_ordered_data => {
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
pub(crate) struct JournalCheckpoint {
    transaction: TransactionId,
}

impl JournalCheckpoint {
    /// Returns the checkpointed transaction sequence.
    #[cfg(unittest)]
    pub(crate) const fn transaction(&self) -> TransactionId {
        self.transaction
    }
}

/// In-memory JBD2 transaction engine owned by one mounted journal.
///
/// This private subobject owns transaction identity, credits, abort state, and
/// checkpoint ordering. `MountedJournal` drives persistence while the same
/// runtime transaction moves through the phases represented here.
pub(crate) struct JournalTransactions {
    state_waiters: WaitQueue,
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

/// Selected FIFO prefix and the persisted boundary that remains live.
///
/// The journal persistence coordinator excludes append/checkpoint until this
/// selection has been written and retired. Merely selecting a prefix frees no log space.
pub(crate) struct JournalCheckpointBatch {
    pub(crate) transactions: Vec<(Arc<RuntimeTransaction>, JournalPersistedCommit)>,
    pub(crate) next_oldest: Option<JournalPersistedCommit>,
}

impl JournalTransactions {
    /// Creates an unbounded transaction engine for isolated tests.
    #[cfg(unittest)]
    pub(crate) fn new(first_transaction: TransactionId) -> Self {
        Self::new_with_credit_limit(first_transaction, u32::MAX)
    }

    /// Creates the transaction engine for one mounted journal.
    pub(crate) fn new_with_credit_limit(
        first_transaction: TransactionId,
        max_reserved_credits: u32,
    ) -> Self {
        Self {
            state_waiters: WaitQueue::new(),
            max_reserved_credits,
            inner: Mutex::new(JournalInner {
                aborted: None,
                next_transaction: first_transaction,
                running: None,
                committing: None,
                checkpoint_transactions: VecDeque::new(),
                checkpointed_through: previous_transaction(first_transaction),
                closing: None,
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
            closing: None,
        };
        Ok(())
    }

    /// Whether one handle fits an empty transaction, independent of current use.
    pub(crate) const fn can_fit_handle(&self, credits: JournalCredits) -> bool {
        credits.get() <= self.max_reserved_credits
    }

    /// Whether an issued transaction has passed durable commit. Its id must be
    /// within the live half of the sequence space, as for checkpoint waits.
    /// A later abort does not undo earlier commits. Read the existing ordered
    /// checkpoint queue/frontier rather than keeping another completion cursor.
    pub(crate) fn is_committed(&self, transaction: TransactionId) -> bool {
        let inner = lock(&self.inner);
        is_checkpointed(&inner, transaction)
            || inner
                .checkpoint_transactions
                .back()
                .is_some_and(|last| transaction_is_same_or_older(transaction, last.id()))
    }

    /// Starts or joins the running transaction, waiting if it has been sealed.
    ///
    /// Do not nest begin while retaining a handle or a lock needed to stop one.
    /// Capacity preflight and begin require the caller's mutation exclusion.
    pub(crate) fn begin(&self, credits: JournalCredits) -> Ext4Result<JournalHandle<'_>> {
        self.ensure_writable()?;
        if !self.can_fit_handle(credits) {
            return Err(Ext4Error::NoSpace);
        }
        let mut inner = loop {
            self.state_waiters.wait_until(|| {
                let inner = lock(&self.inner);
                inner.aborted.is_some() || inner.closing.is_none()
            });
            let inner = lock(&self.inner);
            check_not_aborted(&inner)?;
            if inner.closing.is_none() {
                break inner;
            }
        };

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
            // Another commit request may have advanced this closed handle's
            // transaction before its completion callback acquired the lock.
            return if transaction_is_same_or_older(transaction, running.id()) {
                Ok(None)
            } else {
                Err(Ext4Error::InvalidJournalTransaction)
            };
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
            // Another commit request may have advanced this closed handle's
            // transaction before its completion callback acquired the lock.
            return if transaction_is_same_or_older(transaction, running.id()) {
                Ok(None)
            } else {
                Err(Ext4Error::InvalidJournalTransaction)
            };
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
    pub(crate) fn is_aborted(&self) -> bool {
        lock(&self.inner).aborted.is_some()
    }

    /// Rejects writes after journal abort without maintaining a second flag.
    pub(crate) fn ensure_writable(&self) -> Ext4Result<()> {
        check_not_aborted(&lock(&self.inner))
    }

    /// Validates admission by a caller that serializes complete mutations.
    pub(crate) fn ensure_no_active_handles(&self) -> Ext4Result<()> {
        let inner = lock(&self.inner);
        check_not_aborted(&inner)?;
        if let Some(running) = inner.running.as_ref()
            && running.running_state()?.0 != 0
        {
            return Err(Ext4Error::InvalidJournalTransaction);
        }
        Ok(())
    }

    /// Aborts the journal. After this, new handles and metadata writes fail.
    ///
    /// JBD2 abort is not a cross-operation memory rollback. The running
    /// transaction remains attached so existing handles can stop and release
    /// their reservations, but it can no longer accept updates or commit.
    pub(crate) fn abort(&self, error: Ext4Error) {
        let mut inner = lock(&self.inner);
        inner.aborted.get_or_insert(error);
        drop(inner);
        self.notify_state_waiters();
    }

    /// Freezes the running transaction into its committing phase.
    ///
    /// Persistence is performed separately while the same object remains in
    /// this phase. The transition refuses to run while callers hold handles.
    /// Current running and committing transactions are resolved without
    /// scanning the checkpoint FIFO; only retries for pending ids search it.
    pub(crate) fn force_commit(
        &self,
        transaction: TransactionId,
    ) -> Ext4Result<Arc<RuntimeTransaction>> {
        let mut inner = lock(&self.inner);
        check_not_aborted(&inner)?;

        if let Some(commit) = inner
            .committing
            .as_ref()
            .filter(|commit| commit.id() == transaction)
        {
            return Ok(commit.clone());
        }
        if inner
            .running
            .as_ref()
            .is_some_and(|running| running.id() == transaction)
        {
            // The running slot already identifies this transaction. Searching
            // older checkpoints first would charge each commit for the backlog.
            if inner.committing.is_some() {
                return Err(Ext4Error::JournalBusy);
            }
            let Some(running) = inner.running.as_ref() else {
                return Err(Ext4Error::InvalidJournalTransaction);
            };
            running.freeze_for_commit()?;
            let commit = inner
                .running
                .take()
                .ok_or(Ext4Error::InvalidJournalTransaction)?;
            inner.committing = Some(commit.clone());
            inner.closing = None;
            drop(inner);
            self.notify_state_waiters();
            return Ok(commit);
        }

        // A retry may refer to an older pending transaction even while another
        // one is committing, so preserve that lookup before reporting busy.
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
        Err(Ext4Error::InvalidJournalTransaction)
    }

    /// Snapshots the pending FIFO under the transaction lock, without doing I/O.
    ///
    /// The journal persistence coordinator serializes checkpoint and append until this
    /// batch has completed. Its size is bounded by the occupied journal ring.
    /// Production entries carry persistence evidence before enqueueing; missing
    /// evidence is an invalid transaction, not a commit still in progress.
    pub(crate) fn checkpoint_batch(
        &self,
    ) -> Ext4Result<Vec<(Arc<RuntimeTransaction>, JournalPersistedCommit)>> {
        let inner = lock(&self.inner);
        check_not_aborted(&inner)?;
        inner
            .checkpoint_transactions
            .iter()
            .map(|checkpoint| {
                let persisted = checkpoint
                    .persisted_commit()?
                    .ok_or(Ext4Error::InvalidJournalTransaction)?;
                Ok((checkpoint.clone(), persisted))
            })
            .collect()
    }

    /// Selects enough FIFO work to admit this append and retain a transaction budget.
    ///
    /// Only the selected prefix, its successor and the newest evidence are read;
    /// the unselected suffix is neither snapshotted nor scanned. No I/O runs under
    /// the transaction lock. The journal persistence coordinator serializes selection through
    /// retirement, just as for [`Self::checkpoint_batch`].
    pub(crate) fn checkpoint_batch_for_append(
        &self,
        superblock: &JournalSuperblock,
        commit: &Arc<RuntimeTransaction>,
    ) -> Ext4Result<JournalCheckpointBatch> {
        let inner = lock(&self.inner);
        check_not_aborted(&inner)?;
        if !inner
            .committing
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, commit))
        {
            return Err(Ext4Error::InvalidJournalTransaction);
        }
        let target =
            journal_checkpoint_target_blocks(superblock, commit, self.max_reserved_credits)?;
        let newest = inner
            .checkpoint_transactions
            .back()
            .ok_or(Ext4Error::InvalidJournalTransaction)?
            .persisted_commit()?
            .ok_or(Ext4Error::InvalidJournalTransaction)?;
        let mut pending = inner.checkpoint_transactions.iter();
        let mut transactions = Vec::new();
        while let Some(checkpoint) = pending.next() {
            let persisted = checkpoint
                .persisted_commit()?
                .ok_or(Ext4Error::InvalidJournalTransaction)?;
            transactions.push((checkpoint.clone(), persisted));
            if journal_checkpoint_target_reached(superblock, &persisted, &newest, target)? {
                let next_oldest = pending
                    .next()
                    .map(|next| {
                        next.persisted_commit()?
                            .ok_or(Ext4Error::InvalidJournalTransaction)
                    })
                    .transpose()?;
                return Ok(JournalCheckpointBatch {
                    transactions,
                    next_oldest,
                });
            }
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
    ///
    /// The current owner is checked directly; pending retries retain their
    /// object-identity check without imposing a FIFO scan on every enqueue.
    pub(crate) fn start_checkpoint(
        &self,
        commit: &Arc<RuntimeTransaction>,
        persisted: JournalPersistedCommit,
    ) -> Ext4Result<()> {
        let mut inner = lock(&self.inner);
        check_not_aborted(&inner)?;
        if !inner
            .committing
            .as_ref()
            .is_some_and(|committing| Arc::ptr_eq(committing, commit))
        {
            return if inner
                .checkpoint_transactions
                .iter()
                .any(|checkpoint| Arc::ptr_eq(checkpoint, commit))
            {
                Ok(())
            } else {
                Err(Ext4Error::InvalidJournalTransaction)
            };
        }
        let committing = inner
            .committing
            .take()
            .ok_or(Ext4Error::InvalidJournalTransaction)?;
        if let Err(error) = committing.start_checkpoint(Some(persisted)) {
            inner.committing = Some(committing);
            return Err(error);
        }
        inner.checkpoint_transactions.push_back(committing);
        drop(inner);
        self.notify_state_waiters();
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

    /// Marks the oldest checkpoint transaction complete.
    pub(crate) fn finish_checkpoint(
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
        self.notify_state_waiters();
        Ok(JournalCheckpoint {
            transaction: commit.id(),
        })
    }

    #[cfg(unittest)]
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
        drop(inner);
        self.notify_state_waiters();
        Ok(())
    }

    #[cfg(unittest)]
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
    #[cfg(unittest)]
    pub(crate) fn wait_checkpoint(
        &self,
        transaction: TransactionId,
    ) -> Ext4Result<JournalCheckpoint> {
        self.state_waiters.wait_until(|| {
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
        let result = running.with_running(|running| running.finish_handle(unused_credits));
        drop(inner);
        self.notify_state_waiters();
        result
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

    fn notify_state_waiters(&self) {
        self.state_waiters.notify_all(false);
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

#[cfg(unittest)]
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
pub(crate) struct JournalHandle<'a> {
    journal: &'a JournalTransactions,
    id: TransactionId,
    remaining_credits: u32,
    has_updates: bool,
    closed: bool,
}

impl<'a> JournalHandle<'a> {
    /// Returns the transaction id used by metadata buffers.
    pub(crate) const fn id(&self) -> TransactionId {
        self.id
    }

    /// Returns whether this handle has joined metadata or revoke state.
    pub(crate) const fn has_updates(&self) -> bool {
        self.has_updates
    }

    /// Records successful data I/O whose durability must precede journal commit.
    ///
    /// This does not publish metadata or revoke state. An ordinary error before
    /// metadata access can leave the data dependency pending for a later commit.
    pub(crate) fn require_ordered_data_flush(&mut self) -> Ext4Result<()> {
        self.journal.with_running(self.id, |running| {
            running.ordered_generation = running
                .ordered_generation
                .checked_add(1)
                .ok_or(Ext4Error::Overflow)?;
            Ok(())
        })?;
        Ok(())
    }

    /// Executes a device-wide barrier and acknowledges its dependency snapshot.
    ///
    /// Acknowledges only dependencies registered before this barrier started.
    /// Other handles may register newer writes during I/O; those remain pending.
    /// The barrier holds no journal/phase lock. Failure acknowledges nothing.
    pub(crate) fn flush_ordered_data(
        &mut self,
        barrier_fn: impl FnOnce() -> Ext4Result<()>,
    ) -> Ext4Result<()> {
        let generation = self
            .journal
            .with_running(self.id, |running| Ok(running.ordered_generation))?;
        barrier_fn()?;
        self.journal.with_running(self.id, |running| {
            running.flushed_generation = running.flushed_generation.max(generation);
            Ok(())
        })
    }

    /// Returns remaining credits local to this handle.
    pub(crate) const fn remaining_credits(&self) -> u32 {
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

unittest::path_mod_test!(tests, "../../ut-tests/jbd2/transaction/tests.rs");

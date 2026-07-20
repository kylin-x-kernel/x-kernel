// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Minimal JBD2 transaction handle and credits state.
//!
//! This is not yet the descriptor/commit writer. It establishes the core
//! contract that metadata mutation must pass through a transaction handle with
//! credits and abort checks before touching a metadata buffer.

use alloc::{
    collections::{BTreeMap, BTreeSet},
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

use super::TransactionId;
use crate::{Ext4Error, Ext4Result, FilesystemBlock, InodeNumber};

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
struct RunningTransaction {
    id: TransactionId,
    active_handles: u32,
    reserved_credits: u32,
    used_credits: u32,
    metadata_blocks: BTreeSet<FilesystemBlock>,
    revoked_blocks: BTreeSet<FilesystemBlock>,
    undo_blocks: BTreeMap<FilesystemBlock, Arc<[u8]>>,
    sync_inodes: BTreeSet<InodeNumber>,
}

impl RunningTransaction {
    fn new(id: TransactionId, credits: JournalCredits) -> Self {
        Self {
            id,
            active_handles: 1,
            reserved_credits: credits.get(),
            used_credits: 0,
            metadata_blocks: BTreeSet::new(),
            revoked_blocks: BTreeSet::new(),
            undo_blocks: BTreeMap::new(),
            sync_inodes: BTreeSet::new(),
        }
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

    fn refund(&mut self, credits: JournalCredits) {
        self.used_credits = self.used_credits.saturating_sub(credits.get());
    }

    fn into_commit(self) -> JournalCommit {
        JournalCommit {
            id: self.id,
            reserved_credits: self.reserved_credits,
            used_credits: self.used_credits,
            metadata_blocks: self.metadata_blocks.into_iter().collect(),
            revoked_blocks: self.revoked_blocks.into_iter().collect(),
            undo_blocks: undo_blocks_into_vec(self.undo_blocks),
            sync_inodes: self.sync_inodes.into_iter().collect(),
        }
    }

    fn record_undo_block(&mut self, block: FilesystemBlock, bytes: Arc<[u8]>) {
        self.undo_blocks.entry(block).or_insert(bytes);
    }

    fn cancel_revoke_block(&mut self, block: FilesystemBlock) -> bool {
        self.revoked_blocks.remove(&block)
    }

    fn into_undo(self) -> JournalUndo {
        JournalUndo {
            transaction: self.id,
            blocks: undo_blocks_into_vec(self.undo_blocks),
        }
    }
}

fn undo_blocks_into_vec(
    undo_blocks: BTreeMap<FilesystemBlock, Arc<[u8]>>,
) -> Vec<JournalUndoBlock> {
    undo_blocks
        .into_iter()
        .map(|(block, bytes)| JournalUndoBlock { block, bytes })
        .collect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct JournalRevokeEffect {
    inserted: bool,
    replaced_metadata: bool,
    consumed_credit: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct JournalForgetEffect {
    removed_metadata: bool,
}

#[derive(Debug)]
struct JournalInner {
    aborted: Option<Ext4Error>,
    next_transaction: TransactionId,
    running: Option<RunningTransaction>,
    committed: BTreeMap<TransactionId, JournalCommit>,
    checkpointed: BTreeSet<TransactionId>,
}

/// Metadata owned by a committed in-memory transaction.
///
/// This is the runtime counterpart of a committed JBD2 transaction discovered
/// by recovery scan: checkpoint code consumes it to write the transaction's
/// home metadata blocks and then complete the journal checkpoint state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournalCommit {
    id: TransactionId,
    reserved_credits: u32,
    used_credits: u32,
    metadata_blocks: Vec<FilesystemBlock>,
    revoked_blocks: Vec<FilesystemBlock>,
    undo_blocks: Vec<JournalUndoBlock>,
    sync_inodes: Vec<InodeNumber>,
}

impl JournalCommit {
    /// Returns the committed transaction sequence.
    pub const fn id(&self) -> TransactionId {
        self.id
    }

    /// Returns the credits reserved by handles in this transaction.
    #[cfg(test)]
    pub const fn reserved_credits(&self) -> u32 {
        self.reserved_credits
    }

    /// Returns credits consumed for metadata buffer write access.
    #[cfg(test)]
    pub const fn used_credits(&self) -> u32 {
        self.used_credits
    }

    /// Returns metadata blocks touched by this transaction.
    pub fn metadata_blocks(&self) -> &[FilesystemBlock] {
        &self.metadata_blocks
    }

    /// Returns filesystem blocks revoked by this transaction.
    pub fn revoked_blocks(&self) -> &[FilesystemBlock] {
        &self.revoked_blocks
    }

    /// Returns pre-transaction committed images captured for abort rollback.
    #[cfg(test)]
    pub fn undo_blocks(&self) -> &[JournalUndoBlock] {
        &self.undo_blocks
    }

    /// Returns rollback evidence for this committed in-memory transaction.
    #[cfg(test)]
    pub fn undo(&self) -> JournalUndo {
        JournalUndo {
            transaction: self.id,
            blocks: self.undo_blocks.clone(),
        }
    }

    /// Returns inodes whose fsync must observe this transaction.
    #[cfg(test)]
    pub fn sync_inodes(&self) -> &[InodeNumber] {
        &self.sync_inodes
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

/// Committed copies captured before a running transaction modified metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournalUndo {
    transaction: TransactionId,
    blocks: Vec<JournalUndoBlock>,
}

impl JournalUndo {
    /// Returns the aborted transaction sequence these copies belong to.
    pub const fn transaction(&self) -> TransactionId {
        self.transaction
    }

    /// Returns metadata blocks and their pre-transaction bytes.
    pub fn blocks(&self) -> &[JournalUndoBlock] {
        &self.blocks
    }
}

/// One pre-transaction metadata block image.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournalUndoBlock {
    block: FilesystemBlock,
    bytes: Arc<[u8]>,
}

impl JournalUndoBlock {
    /// Returns the filesystem block number.
    pub const fn block(&self) -> FilesystemBlock {
        self.block
    }

    /// Returns the committed bytes captured before mutation.
    pub fn bytes(&self) -> Arc<[u8]> {
        self.bytes.clone()
    }
}

/// In-memory JBD2 transaction coordinator.
///
/// The current implementation models transaction ownership, credits, abort,
/// and checkpoint state. Actual descriptor and commit block emission will build
/// on this state instead of re-opening metadata mutation paths.
pub struct Journal {
    #[cfg(target_os = "none")]
    checkpoint_waiters: WaitQueue,
    #[cfg(not(target_os = "none"))]
    checkpoint_condition: Condvar,
    inner: Mutex<JournalInner>,
}

impl fmt::Debug for Journal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inner = lock(&self.inner);
        f.debug_struct("Journal")
            .field("inner", &*inner)
            .finish_non_exhaustive()
    }
}

impl Journal {
    /// Creates a journal transaction coordinator.
    pub const fn new(first_transaction: TransactionId) -> Self {
        Self {
            #[cfg(target_os = "none")]
            checkpoint_waiters: WaitQueue::new(),
            #[cfg(not(target_os = "none"))]
            checkpoint_condition: Condvar::new(),
            inner: Mutex::new(JournalInner {
                aborted: None,
                next_transaction: first_transaction,
                running: None,
                committed: BTreeMap::new(),
                checkpointed: BTreeSet::new(),
            }),
        }
    }

    /// Starts or joins the running transaction.
    pub fn begin(&self, credits: JournalCredits) -> Ext4Result<JournalHandle<'_>> {
        let mut inner = lock(&self.inner);
        check_not_aborted(&inner)?;

        let id = match &mut inner.running {
            Some(transaction) => {
                transaction.active_handles = transaction
                    .active_handles
                    .checked_add(1)
                    .ok_or(Ext4Error::Overflow)?;
                transaction.reserve(credits)?;
                transaction.id
            }
            None => {
                let id = inner.next_transaction;
                inner.next_transaction = TransactionId::new(id.get().wrapping_add(1));
                inner.running = Some(RunningTransaction::new(id, credits));
                id
            }
        };

        Ok(JournalHandle {
            journal: self,
            id,
            remaining_credits: credits.get(),
            closed: false,
        })
    }

    /// Returns whether this journal has been aborted.
    #[cfg(test)]
    pub fn is_aborted(&self) -> bool {
        lock(&self.inner).aborted.is_some()
    }

    /// Aborts the journal. After this, new handles and metadata writes fail.
    pub fn abort(&self, error: Ext4Error) -> Option<JournalUndo> {
        let mut inner = lock(&self.inner);
        inner.aborted = Some(error);
        let undo = inner.running.take().map(RunningTransaction::into_undo);
        drop(inner);
        self.notify_checkpoint_waiters();
        undo
    }

    /// Marks a transaction committed once all handles have been dropped.
    ///
    /// This is a state transition placeholder for the future commit writer. It
    /// intentionally refuses to commit while callers still hold handles.
    pub fn force_commit(&self, transaction: TransactionId) -> Ext4Result<JournalCommit> {
        let mut inner = lock(&self.inner);
        check_not_aborted(&inner)?;

        if let Some(commit) = inner.committed.get(&transaction) {
            return Ok(commit.clone());
        }

        if inner
            .running
            .as_ref()
            .is_some_and(|running| running.id == transaction)
        {
            let Some(running) = inner.running.as_ref() else {
                return Err(Ext4Error::InvalidJournalTransaction);
            };
            if running.active_handles != 0 {
                return Err(Ext4Error::JournalBusy);
            }
            let Some(running) = inner.running.take() else {
                return Err(Ext4Error::InvalidJournalTransaction);
            };
            if running.id != transaction {
                inner.running = Some(running);
                return Err(Ext4Error::InvalidJournalTransaction);
            }
            let commit = running.into_commit();
            inner.committed.insert(transaction, commit.clone());
            drop(inner);
            self.notify_checkpoint_waiters();
            return Ok(commit);
        }

        Err(Ext4Error::InvalidJournalTransaction)
    }

    /// Marks a committed transaction checkpointed.
    pub fn finish_checkpoint(&self, commit: &JournalCommit) -> Ext4Result<JournalCheckpoint> {
        let mut inner = lock(&self.inner);
        check_not_aborted(&inner)?;
        if inner.checkpointed.contains(&commit.id) {
            return Ok(JournalCheckpoint {
                transaction: commit.id,
            });
        }
        let stored = inner
            .committed
            .get(&commit.id)
            .ok_or(Ext4Error::InvalidJournalTransaction)?;
        if stored != commit {
            return Err(Ext4Error::InvalidJournalTransaction);
        }
        inner.checkpointed.insert(commit.id);
        drop(inner);
        self.notify_checkpoint_waiters();
        Ok(JournalCheckpoint {
            transaction: commit.id,
        })
    }

    /// Waits for a committed transaction's checkpoint state.
    #[cfg(all(test, not(target_os = "none")))]
    pub fn wait_checkpoint(&self, transaction: TransactionId) -> Ext4Result<JournalCheckpoint> {
        let mut inner = lock(&self.inner);
        loop {
            check_not_aborted(&inner)?;
            if inner.checkpointed.contains(&transaction) {
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
                || inner.checkpointed.contains(&transaction)
                || !is_checkpoint_pending(&inner, transaction)
        });
        let inner = lock(&self.inner);
        check_not_aborted(&inner)?;
        if inner.checkpointed.contains(&transaction) {
            Ok(JournalCheckpoint { transaction })
        } else {
            Err(Ext4Error::InvalidJournalTransaction)
        }
    }

    fn finish_handle(&self, transaction: TransactionId) {
        let mut inner = lock(&self.inner);
        if let Some(running) = &mut inner.running
            && running.id == transaction
        {
            running.active_handles = running.active_handles.saturating_sub(1);
        }
    }

    fn with_running<R>(
        &self,
        transaction: TransactionId,
        f: impl FnOnce(&mut RunningTransaction) -> Ext4Result<R>,
    ) -> Ext4Result<R> {
        let mut inner = lock(&self.inner);
        check_not_aborted(&inner)?;
        let running = inner.running.as_mut().ok_or(Ext4Error::JournalBusy)?;
        if running.id != transaction {
            return Err(Ext4Error::JournalBusy);
        }
        f(running)
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

#[cfg(test)]
fn is_checkpoint_pending(inner: &JournalInner, transaction: TransactionId) -> bool {
    inner.committed.contains_key(&transaction)
        || inner
            .running
            .as_ref()
            .is_some_and(|running| running.id == transaction)
}

/// A live reference to the running transaction plus its remaining credits.
pub struct JournalHandle<'a> {
    journal: &'a Journal,
    id: TransactionId,
    remaining_credits: u32,
    closed: bool,
}

impl<'a> JournalHandle<'a> {
    /// Returns the transaction id used by metadata buffers.
    pub const fn id(&self) -> TransactionId {
        self.id
    }

    /// Returns remaining credits local to this handle.
    pub const fn remaining_credits(&self) -> u32 {
        self.remaining_credits
    }

    /// Reserves additional credits for this handle and transaction.
    #[cfg(test)]
    pub fn reserve_more(&mut self, credits: JournalCredits) -> Ext4Result<()> {
        self.journal.with_running(self.id, |running| {
            running.reserve(credits)?;
            self.remaining_credits = self
                .remaining_credits
                .checked_add(credits.get())
                .ok_or(Ext4Error::Overflow)?;
            Ok(())
        })
    }

    /// Records that fsync must include this inode's metadata.
    pub fn mark_inode_sync(&mut self, inode: InodeNumber) -> Ext4Result<()> {
        self.journal.with_running(self.id, |running| {
            running.sync_inodes.insert(inode);
            Ok(())
        })
    }

    pub(crate) fn consume_metadata_credit(&mut self, block: FilesystemBlock) -> Ext4Result<()> {
        self.journal.with_running(self.id, |running| {
            if running.metadata_blocks.contains(&block) {
                return Ok(());
            }
            let reused_revoke_credit = running.cancel_revoke_block(block);
            if !reused_revoke_credit {
                if self.remaining_credits < JournalCredits::one().get() {
                    return Err(Ext4Error::InsufficientJournalCredits);
                }
                running.consume(JournalCredits::one())?;
                self.remaining_credits -= JournalCredits::one().get();
            }
            running.metadata_blocks.insert(block);
            Ok(())
        })
    }

    pub(crate) fn refund_metadata_credit(&mut self, block: FilesystemBlock) {
        let _ = self.journal.with_running(self.id, |running| {
            if running.metadata_blocks.remove(&block) {
                running.refund(JournalCredits::one());
                self.remaining_credits = self.remaining_credits.saturating_add(1);
            }
            Ok(())
        });
    }

    pub(crate) fn record_undo_block(
        &mut self,
        block: FilesystemBlock,
        bytes: Arc<[u8]>,
    ) -> Ext4Result<()> {
        self.journal.with_running(self.id, |running| {
            running.record_undo_block(block, bytes);
            Ok(())
        })
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

    pub(crate) fn revoke_metadata_block(
        &mut self,
        block: FilesystemBlock,
    ) -> Ext4Result<JournalRevokeEffect> {
        self.journal.with_running(self.id, |running| {
            if running.revoked_blocks.contains(&block) {
                return Ok(JournalRevokeEffect {
                    inserted: false,
                    replaced_metadata: false,
                    consumed_credit: false,
                });
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
            Ok(JournalRevokeEffect {
                inserted: true,
                replaced_metadata: reused_metadata_credit,
                consumed_credit: !reused_metadata_credit,
            })
        })
    }

    pub(crate) fn cancel_revoke_metadata_block(
        &mut self,
        block: FilesystemBlock,
        effect: JournalRevokeEffect,
    ) {
        if !effect.inserted {
            return;
        }
        let _ = self.journal.with_running(self.id, |running| {
            if running.revoked_blocks.remove(&block) {
                if effect.replaced_metadata {
                    running.metadata_blocks.insert(block);
                }
                if effect.consumed_credit {
                    running.refund(JournalCredits::one());
                    self.remaining_credits = self.remaining_credits.saturating_add(1);
                }
            }
            Ok(())
        });
    }

    pub(crate) fn forget_metadata_block_without_revoke(
        &mut self,
        block: FilesystemBlock,
    ) -> Ext4Result<JournalForgetEffect> {
        self.journal.with_running(self.id, |running| {
            let removed_metadata = running.metadata_blocks.remove(&block);
            if removed_metadata {
                running.refund(JournalCredits::one());
                self.remaining_credits = self.remaining_credits.saturating_add(1);
            }
            Ok(JournalForgetEffect { removed_metadata })
        })
    }

    pub(crate) fn cancel_forget_metadata_block_without_revoke(
        &mut self,
        block: FilesystemBlock,
        effect: JournalForgetEffect,
    ) {
        if !effect.removed_metadata {
            return;
        }
        let _ = self.journal.with_running(self.id, |running| {
            if running.metadata_blocks.insert(block) {
                running.consume(JournalCredits::one())?;
                self.remaining_credits = self.remaining_credits.saturating_sub(1);
            }
            Ok(())
        });
    }

    fn finish(&mut self) {
        if !self.closed {
            self.journal.finish_handle(self.id);
            self.closed = true;
        }
    }
}

impl Drop for JournalHandle<'_> {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    #[cfg(not(target_os = "none"))]
    use std::{sync::mpsc, thread, time::Duration};

    use super::*;
    use crate::{FilesystemBlock, InodeNumber};

    #[test]
    fn handles_share_running_transaction_and_consume_credits() {
        let journal = Journal::new(TransactionId::new(3));
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
        let journal = Journal::new(TransactionId::new(8));
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
    fn repeated_metadata_block_consumes_one_credit() {
        let journal = Journal::new(TransactionId::new(10));
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
        assert_eq!(commit.used_credits(), 1);
        assert_eq!(commit.metadata_blocks(), &[FilesystemBlock::new(5)]);
    }

    #[test]
    fn refund_metadata_credit_requires_recorded_block() {
        let journal = Journal::new(TransactionId::new(11));
        let mut handle = journal.begin(JournalCredits::new(1)).unwrap();
        let transaction = handle.id();

        handle
            .consume_metadata_credit(FilesystemBlock::new(5))
            .unwrap();
        handle.refund_metadata_credit(FilesystemBlock::new(6));
        assert_eq!(handle.remaining_credits(), 0);
        handle.refund_metadata_credit(FilesystemBlock::new(5));
        assert_eq!(handle.remaining_credits(), 1);
        drop(handle);

        let commit = journal.force_commit(transaction).unwrap();
        assert_eq!(commit.used_credits(), 0);
        assert!(commit.metadata_blocks().is_empty());
    }

    #[test]
    fn revoke_metadata_block_consumes_one_credit_once() {
        let journal = Journal::new(TransactionId::new(14));
        let mut handle = journal.begin(JournalCredits::new(1)).unwrap();
        let transaction = handle.id();
        let block = FilesystemBlock::new(9);

        handle.revoke_metadata_block(block).unwrap();
        handle.revoke_metadata_block(block).unwrap();
        assert_eq!(handle.remaining_credits(), 0);
        drop(handle);

        let commit = journal.force_commit(transaction).unwrap();
        assert_eq!(commit.used_credits(), 1);
        assert!(commit.metadata_blocks().is_empty());
        assert_eq!(commit.revoked_blocks(), &[block]);
    }

    #[test]
    fn revoke_replaces_metadata_update_in_same_transaction() {
        let journal = Journal::new(TransactionId::new(15));
        let mut handle = journal.begin(JournalCredits::new(1)).unwrap();
        let transaction = handle.id();
        let block = FilesystemBlock::new(10);

        handle.consume_metadata_credit(block).unwrap();
        handle.revoke_metadata_block(block).unwrap();
        assert_eq!(handle.remaining_credits(), 0);
        drop(handle);

        let commit = journal.force_commit(transaction).unwrap();
        assert_eq!(commit.used_credits(), 1);
        assert!(commit.metadata_blocks().is_empty());
        assert_eq!(commit.revoked_blocks(), &[block]);
    }

    #[test]
    fn metadata_update_cancels_revoke_in_same_transaction() {
        let journal = Journal::new(TransactionId::new(16));
        let mut handle = journal.begin(JournalCredits::new(1)).unwrap();
        let transaction = handle.id();
        let block = FilesystemBlock::new(11);

        handle.revoke_metadata_block(block).unwrap();
        handle.consume_metadata_credit(block).unwrap();
        assert_eq!(handle.remaining_credits(), 0);
        drop(handle);

        let commit = journal.force_commit(transaction).unwrap();
        assert_eq!(commit.used_credits(), 1);
        assert_eq!(commit.metadata_blocks(), &[block]);
        assert!(commit.revoked_blocks().is_empty());
    }

    #[test]
    fn abort_blocks_new_and_existing_handles() {
        let journal = Journal::new(TransactionId::new(1));
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
        let journal = Journal::new(TransactionId::new(1));
        let handle = journal.begin(JournalCredits::new(1)).unwrap();
        let transaction = handle.id();

        assert_eq!(
            journal.force_commit(transaction),
            Err(Ext4Error::JournalBusy)
        );
        drop(handle);

        let commit = journal.force_commit(transaction).unwrap();
        let checkpoint = journal.finish_checkpoint(&commit).unwrap();
        assert_eq!(checkpoint.transaction(), transaction);
    }

    #[cfg(not(target_os = "none"))]
    #[test]
    fn wait_checkpoint_blocks_until_checkpoint_finishes() {
        let journal = Arc::new(Journal::new(TransactionId::new(17)));
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
        journal.finish_checkpoint(&commit).unwrap();
        assert_eq!(
            done_receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
            transaction
        );
    }

    #[cfg(not(target_os = "none"))]
    #[test]
    fn wait_checkpoint_wakes_when_journal_aborts() {
        let journal = Arc::new(Journal::new(TransactionId::new(18)));
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
    fn handle_records_sync_inode() {
        let journal = Journal::new(TransactionId::new(5));
        let mut handle = journal.begin(JournalCredits::new(1)).unwrap();

        handle.mark_inode_sync(InodeNumber::new(12)).unwrap();
    }

    #[test]
    fn commit_records_metadata_blocks_credits_and_sync_inodes() {
        let journal = Journal::new(TransactionId::new(9));
        let mut handle = journal.begin(JournalCredits::new(3)).unwrap();
        let transaction = handle.id();

        handle
            .consume_metadata_credit(FilesystemBlock::new(7))
            .unwrap();
        handle
            .consume_metadata_credit(FilesystemBlock::new(3))
            .unwrap();
        handle.mark_inode_sync(InodeNumber::new(12)).unwrap();
        drop(handle);

        let commit = journal.force_commit(transaction).unwrap();
        assert_eq!(commit.id(), transaction);
        assert_eq!(commit.reserved_credits(), 3);
        assert_eq!(commit.used_credits(), 2);
        assert_eq!(
            commit.metadata_blocks(),
            &[FilesystemBlock::new(3), FilesystemBlock::new(7)]
        );
        assert_eq!(commit.sync_inodes(), &[InodeNumber::new(12)]);
    }

    #[test]
    fn force_commit_preserves_undo_blocks_for_later_rollback() {
        let journal = Journal::new(TransactionId::new(10));
        let mut handle = journal.begin(JournalCredits::new(2)).unwrap();
        let transaction = handle.id();
        let block = FilesystemBlock::new(8);
        let first: Arc<[u8]> = Arc::from(&[0x10, 0x11][..]);
        let ignored_later_copy: Arc<[u8]> = Arc::from(&[0x20, 0x21][..]);

        handle.consume_metadata_credit(block).unwrap();
        handle.record_undo_block(block, first.clone()).unwrap();
        handle.record_undo_block(block, ignored_later_copy).unwrap();
        drop(handle);

        let commit = journal.force_commit(transaction).unwrap();

        assert_eq!(commit.undo_blocks().len(), 1);
        assert_eq!(commit.undo_blocks()[0].block(), block);
        assert_eq!(commit.undo_blocks()[0].bytes(), first);
        let undo = commit.undo();
        assert_eq!(undo.transaction(), transaction);
        assert_eq!(undo.blocks(), commit.undo_blocks());
        assert_eq!(journal.force_commit(transaction).unwrap().undo(), undo);
    }

    #[test]
    fn force_commit_is_idempotent_for_committed_transaction() {
        let journal = Journal::new(TransactionId::new(13));
        let handle = journal.begin(JournalCredits::new(1)).unwrap();
        let transaction = handle.id();
        drop(handle);

        let first = journal.force_commit(transaction).unwrap();
        let second = journal.force_commit(transaction).unwrap();

        assert_eq!(first, second);
    }
}

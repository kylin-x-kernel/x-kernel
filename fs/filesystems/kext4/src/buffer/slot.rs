// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{collections::VecDeque, sync::Arc};

#[cfg(not(target_os = "none"))]
use super::sync::Condvar;
#[cfg(target_os = "none")]
use super::sync::WaitQueue;
use super::{
    access::{MetadataBuffer, MetadataWriteAccess, MetadataWriteback},
    sync::{Mutex, lock},
};
use crate::{Ext4Error, Ext4Result, UnsupportedKind, jbd2::TransactionId};

pub(super) enum BufferContents {
    Loading,
    Ready {
        bytes: Arc<[u8]>,
        state: MetadataBufferState,
        checkpoints: VecDeque<CheckpointSnapshot>,
    },
    Failed(Ext4Error),
}

#[derive(Clone)]
pub(super) struct CheckpointSnapshot {
    transaction: TransactionId,
    bytes: Arc<[u8]>,
    needs_home_write: bool,
    is_writeback: bool,
}

/// Journal-facing state for one metadata buffer identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MetadataBufferState {
    /// Buffer contents match stable storage and are not owned by a transaction.
    Clean,
    /// A transaction has obtained write access but has not dirtied the buffer.
    Journaled(TransactionId),
    /// A transaction has created the buffer and must fully initialize it.
    Created(TransactionId),
    /// A transaction has modified the buffer and it must be written back.
    Dirty(TransactionId),
    /// Dirty contents are being checkpointed to their home location.
    #[allow(dead_code)]
    Writeback(TransactionId),
}

fn buffer_state_error() -> Ext4Error {
    Ext4Error::Unsupported(UnsupportedKind::MetadataBufferState)
}

fn transaction_conflict_error() -> Ext4Error {
    Ext4Error::Unsupported(UnsupportedKind::ConcurrentMetadataTransaction)
}

pub(super) struct MetadataBufferSlot {
    #[cfg(target_os = "none")]
    waiters: WaitQueue,
    #[cfg(not(target_os = "none"))]
    condition: Condvar,
    contents: Mutex<BufferContents>,
}

impl MetadataBufferSlot {
    pub(super) fn new() -> Self {
        Self {
            #[cfg(target_os = "none")]
            waiters: WaitQueue::new(),
            #[cfg(not(target_os = "none"))]
            condition: Condvar::new(),
            contents: Mutex::new(BufferContents::Loading),
        }
    }

    pub(super) fn is_reclaimable(&self) -> bool {
        matches!(
            &*lock(&self.contents),
            BufferContents::Ready {
                bytes,
                state: MetadataBufferState::Clean,
                checkpoints,
            } if checkpoints.is_empty() && Arc::strong_count(bytes) == 1
        )
    }

    #[cfg(target_os = "none")]
    pub(super) fn wait_until_loaded(&self) {
        self.waiters
            .wait_until(|| !matches!(&*lock(&self.contents), BufferContents::Loading));
    }

    #[cfg(not(target_os = "none"))]
    pub(super) fn wait_until_loaded(&self) {
        let mut contents = lock(&self.contents);
        while matches!(&*contents, BufferContents::Loading) {
            contents = self
                .condition
                .wait(contents)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    pub(super) fn publish(&self, bytes: Arc<[u8]>) {
        *lock(&self.contents) = BufferContents::Ready {
            bytes,
            state: MetadataBufferState::Clean,
            checkpoints: VecDeque::new(),
        };
        self.notify_all();
    }

    pub(super) fn fail(&self, error: Ext4Error) {
        *lock(&self.contents) = BufferContents::Failed(error);
        self.notify_all();
    }

    pub(super) fn load_result(&self) -> Ext4Result<()> {
        match &*lock(&self.contents) {
            BufferContents::Ready { .. } => Ok(()),
            BufferContents::Failed(error) => Err(*error),
            BufferContents::Loading => {
                unreachable!("metadata buffer must finish loading before its result is observed")
            }
        }
    }

    pub(super) fn result(self: &Arc<Self>) -> Ext4Result<MetadataBuffer> {
        match &*lock(&self.contents) {
            BufferContents::Ready { bytes, state, .. } => {
                if matches!(state, MetadataBufferState::Created(_)) {
                    return Err(buffer_state_error());
                }
                Ok(MetadataBuffer {
                    bytes: bytes.clone(),
                })
            }
            BufferContents::Failed(error) => Err(*error),
            BufferContents::Loading => {
                unreachable!("metadata buffer must finish loading before its result is observed")
            }
        }
    }

    pub(super) fn snapshot_for_commit(
        &self,
        transaction: TransactionId,
    ) -> Ext4Result<MetadataBuffer> {
        let mut contents = lock(&self.contents);
        match &mut *contents {
            BufferContents::Ready {
                bytes,
                state,
                checkpoints,
            } => {
                if let Some(snapshot) = checkpoints
                    .iter()
                    .find(|snapshot| snapshot.transaction == transaction)
                {
                    return Ok(MetadataBuffer {
                        bytes: snapshot.bytes.clone(),
                    });
                }

                let (owner, needs_home_write) = match *state {
                    MetadataBufferState::Journaled(owner) if owner == transaction => (owner, false),
                    MetadataBufferState::Dirty(owner) if owner == transaction => (owner, true),
                    MetadataBufferState::Created(owner) if owner == transaction => {
                        return Err(buffer_state_error());
                    }
                    MetadataBufferState::Clean => return Err(buffer_state_error()),
                    MetadataBufferState::Journaled(_)
                    | MetadataBufferState::Created(_)
                    | MetadataBufferState::Dirty(_)
                    | MetadataBufferState::Writeback(_) => {
                        return Err(transaction_conflict_error());
                    }
                };
                let frozen = bytes.clone();
                checkpoints.push_back(CheckpointSnapshot {
                    transaction: owner,
                    bytes: frozen.clone(),
                    needs_home_write,
                    is_writeback: false,
                });
                *state = MetadataBufferState::Clean;
                Ok(MetadataBuffer { bytes: frozen })
            }
            BufferContents::Failed(error) => Err(*error),
            BufferContents::Loading => {
                unreachable!("metadata buffer must finish loading before commit snapshot")
            }
        }
    }

    pub(super) fn replace_bytes(
        &self,
        transaction: TransactionId,
        new_bytes: Arc<[u8]>,
    ) -> Ext4Result<()> {
        let mut contents = lock(&self.contents);
        match &mut *contents {
            BufferContents::Ready { bytes, state, .. } => {
                if bytes.len() != new_bytes.len() {
                    return Err(Ext4Error::InvalidBufferLength {
                        expected: bytes.len(),
                        actual: new_bytes.len(),
                    });
                }
                match *state {
                    MetadataBufferState::Journaled(owner)
                    | MetadataBufferState::Created(owner)
                    | MetadataBufferState::Dirty(owner)
                        if owner == transaction =>
                    {
                        *bytes = new_bytes;
                        *state = MetadataBufferState::Dirty(transaction);
                        Ok(())
                    }
                    MetadataBufferState::Journaled(_)
                    | MetadataBufferState::Created(_)
                    | MetadataBufferState::Dirty(_)
                    | MetadataBufferState::Writeback(_) => Err(transaction_conflict_error()),
                    MetadataBufferState::Clean => Err(buffer_state_error()),
                }
            }
            BufferContents::Failed(error) => Err(*error),
            BufferContents::Loading => {
                unreachable!("metadata buffer must finish loading before mutation")
            }
        }
    }

    pub(super) fn begin_write_access(
        self: &Arc<Self>,
        transaction: TransactionId,
    ) -> Ext4Result<MetadataWriteAccess> {
        {
            let mut contents = lock(&self.contents);
            match &mut *contents {
                BufferContents::Ready { state, .. } => match *state {
                    MetadataBufferState::Clean => {
                        *state = MetadataBufferState::Journaled(transaction);
                    }
                    MetadataBufferState::Journaled(owner) | MetadataBufferState::Dirty(owner)
                        if owner == transaction => {}
                    MetadataBufferState::Created(owner) if owner == transaction => {
                        return Err(buffer_state_error());
                    }
                    MetadataBufferState::Journaled(_)
                    | MetadataBufferState::Created(_)
                    | MetadataBufferState::Dirty(_) => {
                        return Err(transaction_conflict_error());
                    }
                    MetadataBufferState::Writeback(_) => {
                        return Err(transaction_conflict_error());
                    }
                },
                BufferContents::Failed(error) => return Err(*error),
                BufferContents::Loading => {
                    unreachable!("metadata buffer must finish loading before write access")
                }
            }
        }
        Ok(MetadataWriteAccess {
            slot: self.clone(),
            transaction,
        })
    }

    pub(super) fn begin_create_access(
        self: &Arc<Self>,
        transaction: TransactionId,
        zeroed_bytes: Arc<[u8]>,
    ) -> Ext4Result<MetadataWriteAccess> {
        {
            let mut contents = lock(&self.contents);
            match &mut *contents {
                BufferContents::Ready {
                    bytes,
                    state,
                    checkpoints,
                } => match *state {
                    MetadataBufferState::Clean
                        if checkpoints.is_empty()
                            || checkpoints_allow_create_reuse(checkpoints) =>
                    {
                        *bytes = zeroed_bytes;
                        *state = MetadataBufferState::Created(transaction);
                    }
                    MetadataBufferState::Clean => {
                        return Err(transaction_conflict_error());
                    }
                    MetadataBufferState::Created(owner) if owner == transaction => {}
                    MetadataBufferState::Journaled(owner) | MetadataBufferState::Dirty(owner)
                        if owner == transaction =>
                    {
                        return Err(buffer_state_error());
                    }
                    MetadataBufferState::Journaled(_)
                    | MetadataBufferState::Created(_)
                    | MetadataBufferState::Dirty(_)
                    | MetadataBufferState::Writeback(_) => {
                        return Err(transaction_conflict_error());
                    }
                },
                BufferContents::Failed(error) => return Err(*error),
                BufferContents::Loading => {
                    unreachable!("metadata buffer must finish loading before create access")
                }
            }
        }
        Ok(MetadataWriteAccess {
            slot: self.clone(),
            transaction,
        })
    }

    pub(super) fn mark_dirty(&self, transaction: TransactionId) -> Ext4Result<()> {
        let mut contents = lock(&self.contents);
        match &mut *contents {
            BufferContents::Ready { state, .. } => match *state {
                MetadataBufferState::Journaled(owner) | MetadataBufferState::Dirty(owner)
                    if owner == transaction =>
                {
                    *state = MetadataBufferState::Dirty(transaction);
                    Ok(())
                }
                MetadataBufferState::Created(owner) if owner == transaction => {
                    Err(buffer_state_error())
                }
                MetadataBufferState::Journaled(_)
                | MetadataBufferState::Created(_)
                | MetadataBufferState::Dirty(_)
                | MetadataBufferState::Writeback(_) => Err(transaction_conflict_error()),
                MetadataBufferState::Clean => Err(buffer_state_error()),
            },
            BufferContents::Failed(error) => Err(*error),
            BufferContents::Loading => {
                unreachable!("metadata buffer must finish loading before dirtying")
            }
        }
    }

    pub(super) fn begin_writeback(
        self: &Arc<Self>,
        transaction: TransactionId,
    ) -> Ext4Result<MetadataWriteback> {
        let mut contents = lock(&self.contents);
        match &mut *contents {
            BufferContents::Ready {
                bytes,
                state,
                checkpoints,
            } => {
                if !checkpoints.is_empty() {
                    return Err(transaction_conflict_error());
                }
                match *state {
                    MetadataBufferState::Dirty(owner) if owner == transaction => {
                        let frozen = bytes.clone();
                        checkpoints.push_back(CheckpointSnapshot {
                            transaction,
                            bytes: frozen.clone(),
                            needs_home_write: true,
                            is_writeback: true,
                        });
                        *state = MetadataBufferState::Clean;
                        Ok(MetadataWriteback {
                            slot: self.clone(),
                            transaction,
                            bytes: frozen,
                        })
                    }
                    MetadataBufferState::Journaled(owner)
                    | MetadataBufferState::Created(owner)
                    | MetadataBufferState::Dirty(owner)
                    | MetadataBufferState::Writeback(owner)
                        if owner != transaction =>
                    {
                        Err(transaction_conflict_error())
                    }
                    MetadataBufferState::Clean
                    | MetadataBufferState::Created(_)
                    | MetadataBufferState::Journaled(_)
                    | MetadataBufferState::Writeback(_) => Err(buffer_state_error()),
                    MetadataBufferState::Dirty(_) => unreachable!("dirty owner checked above"),
                }
            }
            BufferContents::Failed(error) => Err(*error),
            BufferContents::Loading => {
                unreachable!("metadata buffer must finish loading before writeback")
            }
        }
    }

    pub(super) fn begin_checkpoint(
        self: &Arc<Self>,
        transaction: TransactionId,
    ) -> Ext4Result<Option<MetadataWriteback>> {
        let mut contents = lock(&self.contents);
        match &mut *contents {
            BufferContents::Ready {
                bytes,
                state,
                checkpoints,
            } => {
                if let Some(snapshot) = checkpoints.front_mut() {
                    if snapshot.transaction == transaction {
                        if !snapshot.needs_home_write {
                            checkpoints.pop_front();
                            self.notify_all();
                            return Ok(None);
                        }
                        if snapshot.is_writeback {
                            return Err(buffer_state_error());
                        }
                        snapshot.is_writeback = true;
                        return Ok(Some(MetadataWriteback {
                            slot: self.clone(),
                            transaction,
                            bytes: snapshot.bytes.clone(),
                        }));
                    }
                    if checkpoints
                        .iter()
                        .any(|snapshot| snapshot.transaction == transaction)
                        || owns_transaction(*state, transaction)
                    {
                        return Err(Ext4Error::JournalBusy);
                    }
                }

                match *state {
                    MetadataBufferState::Dirty(owner) if owner == transaction => {
                        if !checkpoints.is_empty() {
                            return Err(Ext4Error::JournalBusy);
                        }
                        let frozen = bytes.clone();
                        checkpoints.push_back(CheckpointSnapshot {
                            transaction,
                            bytes: frozen.clone(),
                            needs_home_write: true,
                            is_writeback: true,
                        });
                        *state = MetadataBufferState::Clean;
                        Ok(Some(MetadataWriteback {
                            slot: self.clone(),
                            transaction,
                            bytes: frozen,
                        }))
                    }
                    MetadataBufferState::Journaled(owner) if owner == transaction => {
                        if !checkpoints.is_empty() {
                            return Err(Ext4Error::JournalBusy);
                        }
                        *state = MetadataBufferState::Clean;
                        Ok(None)
                    }
                    MetadataBufferState::Created(owner) if owner == transaction => {
                        Err(buffer_state_error())
                    }
                    MetadataBufferState::Clean => Ok(None),
                    MetadataBufferState::Journaled(_)
                    | MetadataBufferState::Created(_)
                    | MetadataBufferState::Dirty(_)
                    | MetadataBufferState::Writeback(_) => Err(transaction_conflict_error()),
                }
            }
            BufferContents::Failed(error) => Err(*error),
            BufferContents::Loading => {
                unreachable!("metadata buffer must finish loading before checkpoint")
            }
        }
    }

    pub(super) fn finish_writeback(&self, transaction: TransactionId) -> Ext4Result<()> {
        let mut contents = lock(&self.contents);
        match &mut *contents {
            BufferContents::Ready {
                state, checkpoints, ..
            } => {
                let Some(snapshot) = checkpoints.front() else {
                    return Err(buffer_state_error());
                };
                if snapshot.transaction != transaction {
                    return Err(transaction_conflict_error());
                }
                if !snapshot.is_writeback {
                    return Err(buffer_state_error());
                }
                checkpoints.pop_front();
                if matches!(*state, MetadataBufferState::Writeback(_)) {
                    *state = MetadataBufferState::Clean;
                }
                self.notify_all();
                Ok(())
            }
            BufferContents::Failed(error) => Err(*error),
            BufferContents::Loading => {
                unreachable!("metadata buffer must finish loading before checkpoint")
            }
        }
    }

    pub(super) fn restore_undo(
        &self,
        transaction: TransactionId,
        undo_bytes: Arc<[u8]>,
    ) -> Ext4Result<()> {
        let mut contents = lock(&self.contents);
        match &mut *contents {
            BufferContents::Ready {
                bytes,
                state,
                checkpoints,
            } => {
                if bytes.len() != undo_bytes.len() {
                    return Err(Ext4Error::InvalidBufferLength {
                        expected: bytes.len(),
                        actual: undo_bytes.len(),
                    });
                }
                if let Some(snapshot) = checkpoints.front() {
                    if snapshot.transaction != transaction
                        || checkpoints.len() != 1
                        || !matches!(*state, MetadataBufferState::Clean)
                    {
                        return Err(transaction_conflict_error());
                    }
                    checkpoints.pop_front();
                    self.notify_all();
                }
                match *state {
                    MetadataBufferState::Journaled(owner)
                    | MetadataBufferState::Created(owner)
                    | MetadataBufferState::Dirty(owner)
                        if owner == transaction =>
                    {
                        *bytes = undo_bytes;
                        *state = MetadataBufferState::Clean;
                        Ok(())
                    }
                    MetadataBufferState::Clean => {
                        *bytes = undo_bytes;
                        Ok(())
                    }
                    MetadataBufferState::Journaled(_)
                    | MetadataBufferState::Created(_)
                    | MetadataBufferState::Dirty(_)
                    | MetadataBufferState::Writeback(_) => Err(transaction_conflict_error()),
                }
            }
            BufferContents::Failed(error) => Err(*error),
            BufferContents::Loading => {
                unreachable!("metadata buffer must finish loading before undo restore")
            }
        }
    }

    pub(super) fn forget_or_revoke_checkpointed(
        &self,
        transaction: TransactionId,
    ) -> Ext4Result<()> {
        let mut contents = lock(&self.contents);
        match &mut *contents {
            BufferContents::Ready {
                state, checkpoints, ..
            } => match *state {
                MetadataBufferState::Journaled(owner)
                | MetadataBufferState::Created(owner)
                | MetadataBufferState::Dirty(owner)
                    if owner == transaction =>
                {
                    if checkpoints.iter().any(|snapshot| snapshot.is_writeback) {
                        return Err(Ext4Error::JournalBusy);
                    }
                    for snapshot in checkpoints {
                        snapshot.needs_home_write = false;
                    }
                    *state = MetadataBufferState::Clean;
                    self.notify_all();
                    Ok(())
                }
                MetadataBufferState::Clean => {
                    if checkpoints.iter().any(|snapshot| snapshot.is_writeback) {
                        return Err(Ext4Error::JournalBusy);
                    }
                    for snapshot in checkpoints {
                        snapshot.needs_home_write = false;
                    }
                    self.notify_all();
                    Ok(())
                }
                MetadataBufferState::Journaled(_)
                | MetadataBufferState::Created(_)
                | MetadataBufferState::Dirty(_)
                | MetadataBufferState::Writeback(_) => Err(transaction_conflict_error()),
            },
            BufferContents::Failed(error) => Err(*error),
            BufferContents::Loading => {
                unreachable!("metadata buffer must finish loading before forget")
            }
        }
    }

    pub(super) fn fail_writeback(&self, error: Ext4Error) {
        self.fail(error);
    }

    #[cfg(test)]
    pub(super) fn state(&self) -> Ext4Result<MetadataBufferState> {
        match &*lock(&self.contents) {
            BufferContents::Ready {
                state, checkpoints, ..
            } => {
                if let Some(snapshot) = checkpoints.front()
                    && snapshot.is_writeback
                    && matches!(state, MetadataBufferState::Clean)
                {
                    return Ok(MetadataBufferState::Writeback(snapshot.transaction));
                }
                Ok(*state)
            }
            BufferContents::Failed(error) => Err(*error),
            BufferContents::Loading => {
                unreachable!("metadata buffer must finish loading before state inspection")
            }
        }
    }

    #[cfg(target_os = "none")]
    fn notify_all(&self) {
        self.waiters.notify_all(false);
    }

    #[cfg(not(target_os = "none"))]
    fn notify_all(&self) {
        self.condition.notify_all();
    }
}

fn checkpoints_allow_create_reuse(checkpoints: &VecDeque<CheckpointSnapshot>) -> bool {
    checkpoints
        .iter()
        .all(|snapshot| !snapshot.needs_home_write && !snapshot.is_writeback)
}

fn owns_transaction(state: MetadataBufferState, transaction: TransactionId) -> bool {
    match state {
        MetadataBufferState::Journaled(owner)
        | MetadataBufferState::Created(owner)
        | MetadataBufferState::Dirty(owner)
        | MetadataBufferState::Writeback(owner) => owner == transaction,
        MetadataBufferState::Clean => false,
    }
}

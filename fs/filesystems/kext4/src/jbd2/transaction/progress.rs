// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Commit admission and waiting without retaining transaction state locks.

use super::*;

impl JournalTransactions {
    /// Seals the target against new handles, waits for active handles, and
    /// then freezes it. A different committing transaction returns JournalBusy;
    /// its persistence owner must advance it before retrying. Persisted targets need no I/O.
    ///
    /// Call without a handle or any lock required by a handle's stop path.
    /// Persistence callers additionally serialize disk work in MountedJournal.
    pub(crate) fn prepare_commit(
        &self,
        transaction: TransactionId,
    ) -> Ext4Result<Option<Arc<RuntimeTransaction>>> {
        {
            let mut inner = lock(&self.inner);
            check_not_aborted(&inner)?;
            if inner
                .running
                .as_ref()
                .is_some_and(|running| running.id() == transaction)
            {
                inner.closing = Some(transaction);
            }
        }
        loop {
            {
                let inner = lock(&self.inner);
                check_not_aborted(&inner)?;
                let is_current = inner
                    .running
                    .as_ref()
                    .is_some_and(|entry| entry.id() == transaction)
                    || inner
                        .committing
                        .as_ref()
                        .is_some_and(|entry| entry.id() == transaction);
                // Resolve current slots first, then use the existing durable
                // frontier instead of scanning the checkpoint backlog.
                if !is_current {
                    if is_checkpointed(&inner, transaction)
                        || inner.checkpoint_transactions.back().is_some_and(|last| {
                            transaction_is_same_or_older(transaction, last.id())
                        })
                    {
                        return Ok(None);
                    }
                    return Err(Ext4Error::InvalidJournalTransaction);
                }
            }
            match self.force_commit(transaction) {
                Ok(commit) => return Ok(Some(commit)),
                Err(Ext4Error::JournalBusy) => {
                    let mut inner = lock(&self.inner);
                    check_not_aborted(&inner)?;
                    if inner
                        .committing
                        .as_ref()
                        .is_some_and(|entry| entry.id() != transaction)
                    {
                        // The caller may own the persistence lock needed to
                        // advance the previous commit. Never wait on it here.
                        if inner.closing == Some(transaction) {
                            inner.closing = None;
                        }
                        drop(inner);
                        self.notify_state_waiters();
                        return Err(Ext4Error::JournalBusy);
                    }
                    drop(inner);
                    self.state_waiters
                        .wait_until(|| self.can_prepare_commit(transaction));
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn can_prepare_commit(&self, transaction: TransactionId) -> bool {
        let inner = lock(&self.inner);
        if inner.aborted.is_some()
            || is_checkpointed(&inner, transaction)
            || inner.committing.is_some()
        {
            return true;
        }
        match inner
            .running
            .as_ref()
            .filter(|entry| entry.id() == transaction)
        {
            Some(running) => {
                inner.committing.is_none()
                    && running.running_state().is_ok_and(|(active, _)| active == 0)
            }
            None => true,
        }
    }

    /// Seals a transaction whose projected reservation exceeds the limit.
    /// The caller drives its commit without holding an active handle. Checking
    /// capacity and the subsequent begin still share the caller's mutation
    /// exclusion; this preflight is not an independently reserved credit ticket.
    pub(crate) fn prepare_reservation(
        &self,
        credits: JournalCredits,
    ) -> Ext4Result<Option<TransactionId>> {
        let mut inner = lock(&self.inner);
        check_not_aborted(&inner)?;
        if !self.can_fit_handle(credits) {
            return Err(Ext4Error::NoSpace);
        }
        let Some(running) = inner.running.as_ref() else {
            return Ok(None);
        };
        let (_, reserved) = running.running_state()?;
        let projected = reserved
            .checked_add(credits.get())
            .ok_or(Ext4Error::Overflow)?;
        if projected <= self.max_reserved_credits && inner.closing.is_none() {
            return Ok(None);
        }
        let transaction = running.id();
        inner.closing = Some(transaction);
        Ok(Some(transaction))
    }
}

unittest::path_mod_test!(
    tests,
    "../../../ut-tests/jbd2/transaction/progress/tests.rs"
);

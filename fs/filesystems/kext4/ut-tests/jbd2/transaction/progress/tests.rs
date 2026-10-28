// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Transaction progress without the outer mount lock.

use unittest::def_test;

use super::*;

#[def_test]
fn running_barrier_preserves_dependencies_registered_during_io() {
    let journal = JournalTransactions::new(TransactionId::new(10));
    let mut first = journal.begin(JournalCredits::one()).unwrap();
    let mut second = journal.begin(JournalCredits::one()).unwrap();
    let transaction = first.id();
    first.require_ordered_data_flush().unwrap();
    first
        .flush_ordered_data(|| {
            // A separate handle finishes another write while this barrier is in flight.
            second.require_ordered_data_flush()
        })
        .unwrap();
    drop(first);
    drop(second);
    let frozen = journal.prepare_commit(transaction).unwrap().unwrap();
    assert!(frozen.has_unflushed_ordered_data().unwrap());
}

#[def_test]
fn overlapping_barriers_do_not_move_acknowledgement_backwards() {
    let journal = JournalTransactions::new(TransactionId::new(11));
    let mut first = journal.begin(JournalCredits::one()).unwrap();
    let mut second = journal.begin(JournalCredits::one()).unwrap();
    let transaction = first.id();
    first.require_ordered_data_flush().unwrap();
    first
        .flush_ordered_data(|| {
            second.require_ordered_data_flush()?;
            second.flush_ordered_data(|| Ok(()))
        })
        .unwrap();
    drop(first);
    drop(second);
    let frozen = journal.prepare_commit(transaction).unwrap().unwrap();
    assert!(!frozen.has_unflushed_ordered_data().unwrap());
}

#[def_test]
fn failed_running_barrier_keeps_dependency_for_commit_retry() {
    let journal = JournalTransactions::new(TransactionId::new(12));
    let mut handle = journal.begin(JournalCredits::one()).unwrap();
    let transaction = handle.id();
    handle.require_ordered_data_flush().unwrap();
    assert_eq!(
        handle.flush_ordered_data(|| Err(Ext4Error::OutOfBounds)),
        Err(Ext4Error::OutOfBounds)
    );
    drop(handle);
    let frozen = journal.prepare_commit(transaction).unwrap().unwrap();
    assert!(frozen.has_unflushed_ordered_data().unwrap());
    frozen.flush_ordered_data(|| Ok(())).unwrap();
    assert!(!frozen.has_unflushed_ordered_data().unwrap());
}

#[def_test]
fn commit_seals_admission_until_existing_handle_stops() {
    let journal = Arc::new(JournalTransactions::new(TransactionId::new(20)));
    let handle = journal.begin(JournalCredits::one()).unwrap();
    let transaction = handle.id();
    let committed = Arc::new(Mutex::new(None));
    let committer = {
        let journal = journal.clone();
        let committed = committed.clone();
        ktask::spawn(move || {
            *lock(&committed) = Some(
                journal
                    .prepare_commit(transaction)
                    .map(|commit| commit.unwrap().id()),
            );
        })
    };
    // Observe the actual closing transition, rather than relying on a delay.
    while lock(&journal.inner).closing != Some(transaction) {
        ktask::yield_now();
    }
    assert!(lock(&committed).is_none());
    let admitted = Arc::new(Mutex::new(None));
    let beginner = {
        let journal = journal.clone();
        let admitted = admitted.clone();
        ktask::spawn(move || {
            let handle = journal.begin(JournalCredits::one()).unwrap();
            *lock(&admitted) = Some(handle.id());
        })
    };
    handle.stop().unwrap();
    committer.join();
    beginner.join();
    assert_eq!(lock(&committed).take().unwrap(), Ok(transaction));
    assert_eq!(
        lock(&admitted).take().unwrap(),
        next_transaction(transaction)
    );
}

#[def_test]
fn abort_releases_both_commit_and_admission_waiters() {
    let journal = Arc::new(JournalTransactions::new_with_credit_limit(
        TransactionId::new(30),
        3,
    ));
    let handle = journal.begin(JournalCredits::one()).unwrap();
    let transaction = handle.id();
    // Credit pressure seals even when a handle is still active.
    assert_eq!(
        journal.prepare_reservation(JournalCredits::new(3)).unwrap(),
        Some(transaction)
    );
    let commit_result = Arc::new(Mutex::new(None));
    let begin_result = Arc::new(Mutex::new(None));
    let committer = {
        let journal = journal.clone();
        let result = commit_result.clone();
        ktask::spawn(move || {
            *lock(&result) = Some(journal.prepare_commit(transaction).map(|_| ()));
        })
    };
    let beginner = {
        let journal = journal.clone();
        let result = begin_result.clone();
        ktask::spawn(move || {
            *lock(&result) = Some(journal.begin(JournalCredits::one()).map(|_| ()));
        })
    };
    // No other task owns these result locks or transaction state while we
    // observe them. Blocked therefore means both reached the journal wait.
    for _ in 0..100_000 {
        if committer.state() == ktask::TaskState::Blocked
            && beginner.state() == ktask::TaskState::Blocked
        {
            break;
        }
        ktask::yield_now();
    }
    let both_blocked = committer.state() == ktask::TaskState::Blocked
        && beginner.state() == ktask::TaskState::Blocked;
    journal.abort(Ext4Error::OutOfBounds);
    for _ in 0..100_000 {
        if committer.state() == ktask::TaskState::Exited
            && beginner.state() == ktask::TaskState::Exited
        {
            break;
        }
        ktask::yield_now();
    }
    let both_released = committer.state() == ktask::TaskState::Exited
        && beginner.state() == ktask::TaskState::Exited;
    // Rescue tasks if the notification regresses, so failure does not hang
    // the entire kernel test suite.
    journal.notify_state_waiters();
    committer.join();
    beginner.join();
    assert!(both_blocked);
    assert!(both_released);
    assert_eq!(
        lock(&commit_result).take().unwrap(),
        Err(Ext4Error::JournalAborted)
    );
    assert_eq!(
        lock(&begin_result).take().unwrap(),
        Err(Ext4Error::JournalAborted)
    );
    handle.stop().unwrap();
}

#[def_test]
fn credit_pressure_seals_then_waits_for_the_active_handle() {
    let journal = JournalTransactions::new_with_credit_limit(TransactionId::new(40), 3);
    let handle = journal.begin(JournalCredits::new(2)).unwrap();
    let transaction = handle.id();
    assert_eq!(
        journal.prepare_reservation(JournalCredits::new(2)).unwrap(),
        Some(transaction)
    );
    assert_eq!(lock(&journal.inner).closing, Some(transaction));
    handle.stop().unwrap();
    assert_eq!(
        journal.prepare_commit(transaction).unwrap().unwrap().id(),
        transaction
    );
    assert!(lock(&journal.inner).closing.is_none());
    assert_eq!(
        journal.begin(JournalCredits::one()).unwrap().id(),
        next_transaction(transaction)
    );
}

#[def_test]
fn prepare_commit_rejects_previous_committing_slot_without_sealing_admission() {
    let journal = JournalTransactions::new(TransactionId::new(50));
    let first_id = journal.begin(JournalCredits::one()).unwrap().id();
    let first = journal.prepare_commit(first_id).unwrap().unwrap();
    let second_id = journal.begin(JournalCredits::one()).unwrap().id();
    assert!(matches!(
        journal.prepare_commit(second_id),
        Err(Ext4Error::JournalBusy)
    ));
    assert!(lock(&journal.inner).closing.is_none());
    assert_eq!(
        journal.begin(JournalCredits::one()).unwrap().id(),
        second_id
    );
    journal.start_checkpoint_for_test(&first).unwrap();
    assert_eq!(
        journal.prepare_commit(second_id).unwrap().unwrap().id(),
        second_id
    );
}

#[def_test]
fn closed_handle_completion_tolerates_a_new_running_transaction() {
    let journal = JournalTransactions::new(TransactionId::new(60));
    let transaction = journal.begin(JournalCredits::one()).unwrap().id();
    journal.prepare_commit(transaction).unwrap().unwrap();
    let next = journal.begin(JournalCredits::one()).unwrap();
    assert_eq!(
        journal.transaction_to_commit_after_handle(transaction),
        Ok(None)
    );
    assert_eq!(journal.idle_running_transaction(transaction), Ok(None));
    assert_eq!(
        journal.idle_running_transaction(next_transaction(next.id())),
        Err(Ext4Error::InvalidJournalTransaction)
    );
}

#[def_test]
fn invalid_target_does_not_wait_on_an_unrelated_committing_transaction() {
    let journal = JournalTransactions::new(TransactionId::new(70));
    let transaction = journal.begin(JournalCredits::one()).unwrap().id();
    journal.prepare_commit(transaction).unwrap().unwrap();
    assert_eq!(
        journal
            .prepare_commit(next_transaction(transaction))
            .map(|_| ()),
        Err(Ext4Error::InvalidJournalTransaction)
    );
}

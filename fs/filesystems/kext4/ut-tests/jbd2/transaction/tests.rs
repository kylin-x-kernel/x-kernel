// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;
use crate::FilesystemBlock;

#[def_test]
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

#[def_test]
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

#[def_test]
fn reserve_more_rejects_transaction_limit_overflow() {
    let journal = JournalTransactions::new_with_credit_limit(TransactionId::new(8), 3);
    let mut handle = journal.begin(JournalCredits::new(2)).unwrap();

    assert_eq!(
        handle.reserve_more(JournalCredits::new(2)),
        Err(Ext4Error::InsufficientJournalCredits)
    );
    assert_eq!(handle.remaining_credits(), 2);
}

#[def_test]
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

#[def_test]
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

#[def_test]
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

#[def_test]
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

#[def_test]
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

#[def_test]
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

#[def_test]
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

#[def_test]
fn wait_checkpoint_blocks_until_checkpoint_finishes() {
    use core::sync::atomic::{AtomicBool, Ordering};
    let journal = Arc::new(JournalTransactions::new(TransactionId::new(17)));
    let handle = journal.begin(JournalCredits::new(1)).unwrap();
    let transaction = handle.id();
    drop(handle);
    let commit = journal.force_commit(transaction).unwrap();
    let ready = Arc::new(AtomicBool::new(false));
    let done = Arc::new(Mutex::new(None));
    let waiter = {
        let journal = journal.clone();
        let ready = ready.clone();
        let done = done.clone();
        ktask::spawn(move || {
            ready.store(true, Ordering::Release);
            *lock(&done) = Some(
                journal
                    .wait_checkpoint(transaction)
                    .map(|checkpoint| checkpoint.transaction()),
            );
        })
    };
    while !ready.load(Ordering::Acquire) {
        ktask::yield_now();
    }
    ktask::sleep(ktime_types::TimeSpan::from_millis(20));
    assert!(
        lock(&done).is_none(),
        "checkpoint wait returned before completion"
    );
    journal.finish_checkpoint_for_test(&commit).unwrap();
    waiter.join();
    assert_eq!(lock(&done).take().unwrap(), Ok(transaction));
}

#[def_test]
fn wait_checkpoint_wakes_when_journal_aborts() {
    use core::sync::atomic::{AtomicBool, Ordering};
    let journal = Arc::new(JournalTransactions::new(TransactionId::new(17)));
    let handle = journal.begin(JournalCredits::new(1)).unwrap();
    let transaction = handle.id();
    drop(handle);
    let _commit = journal.force_commit(transaction).unwrap();
    let ready = Arc::new(AtomicBool::new(false));
    let done = Arc::new(Mutex::new(None));
    let waiter = {
        let journal = journal.clone();
        let ready = ready.clone();
        let done = done.clone();
        ktask::spawn(move || {
            ready.store(true, Ordering::Release);
            *lock(&done) = Some(
                journal
                    .wait_checkpoint(transaction)
                    .map(|checkpoint| checkpoint.transaction()),
            );
        })
    };
    while !ready.load(Ordering::Acquire) {
        ktask::yield_now();
    }
    ktask::sleep(ktime_types::TimeSpan::from_millis(20));
    assert!(
        lock(&done).is_none(),
        "checkpoint wait returned before completion"
    );
    journal.abort(Ext4Error::Device(block::DriverError::Io));
    waiter.join();
    assert_eq!(lock(&done).take().unwrap(), Err(Ext4Error::JournalAborted));
}

#[def_test]
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

#[def_test]
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

#[def_test]
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

#[def_test]
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

#[def_test]
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

#[def_test]
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

#[def_test]
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

#[def_test]
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

#[def_test]
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
        journal.prepare_reservation(JournalCredits::new(1)).unwrap(),
        None
    );
    assert_eq!(
        journal.prepare_reservation(JournalCredits::new(3)).unwrap(),
        Some(transaction)
    );
}

#[def_test]
fn admission_rejects_one_handle_that_exceeds_limit() {
    let journal = JournalTransactions::new_with_credit_limit(TransactionId::new(33), 4);

    assert_eq!(
        journal.prepare_reservation(JournalCredits::new(4)).unwrap(),
        None
    );
    assert_eq!(
        journal.prepare_reservation(JournalCredits::new(5)),
        Err(Ext4Error::NoSpace)
    );
    assert_eq!(
        journal.begin(JournalCredits::new(5)).map(|_| ()),
        Err(Ext4Error::NoSpace)
    );
    assert_eq!(journal.running_transaction().unwrap(), None);
}

#[def_test]
fn admission_allows_exact_capacity_and_waits_above_limit() {
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
        journal.prepare_reservation(JournalCredits::new(1)).unwrap(),
        None
    );
    assert_eq!(
        journal.prepare_reservation(JournalCredits::new(2)).unwrap(),
        None
    );
    assert_eq!(
        journal.prepare_reservation(JournalCredits::new(3)),
        Ok(Some(transaction))
    );

    drop(handle);
    assert_eq!(
        journal.prepare_reservation(JournalCredits::new(3)).unwrap(),
        Some(transaction)
    );
}

#[def_test]
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

#[def_test]
fn reserved_commit_waits_for_handle_stop_or_abort() {
    for should_abort in [false, true] {
        let journal = Arc::new(JournalTransactions::new_with_credit_limit(
            TransactionId::new(60),
            4,
        ));
        let mut handle = journal.begin(JournalCredits::new(2)).unwrap();
        let transaction = handle.id();
        handle
            .consume_metadata_credit(FilesystemBlock::new(1))
            .unwrap();
        handle
            .consume_metadata_credit(FilesystemBlock::new(2))
            .unwrap();
        assert_eq!(
            journal.prepare_reservation(JournalCredits::new(3)),
            Ok(Some(transaction))
        );
        let result = Arc::new(Mutex::new(None));
        let waiter = {
            let journal = journal.clone();
            let result = result.clone();
            ktask::spawn(move || {
                *lock(&result) = Some(
                    journal
                        .prepare_commit(transaction)
                        .map(|commit| commit.map(|entry| entry.id())),
                );
            })
        };
        while waiter.state() != ktask::TaskState::Blocked {
            assert_ne!(waiter.state(), ktask::TaskState::Exited);
            ktask::yield_now();
        }
        assert!(lock(&result).is_none());
        if should_abort {
            journal.abort(Ext4Error::Device(block::DriverError::Io));
        }
        drop(handle);
        waiter.join();
        assert_eq!(
            lock(&result).take().unwrap(),
            if should_abort {
                Err(Ext4Error::JournalAborted)
            } else {
                Ok(Some(transaction))
            }
        );
        if should_abort {
            assert_eq!(
                journal.begin(JournalCredits::new(5)).map(|_| ()),
                Err(Ext4Error::JournalAborted)
            );
        }
    }
}

#[def_test]
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

#[def_test]
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

#[def_test]
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

#[def_test]
fn committed_query_follows_existing_transaction_lifecycle_across_wrap() {
    let journal = JournalTransactions::new(TransactionId::new(u32::MAX));
    let first = journal.begin(JournalCredits::new(0)).unwrap();
    let first_id = first.id();
    assert!(!journal.is_committed(first_id));
    first.stop().unwrap();
    assert!(!journal.is_committed(first_id));
    let first = journal.force_commit(first_id).unwrap();
    assert!(!journal.is_committed(first_id));
    journal.start_checkpoint_for_test(&first).unwrap();
    assert!(journal.is_committed(first_id));
    let second = journal.begin(JournalCredits::new(0)).unwrap();
    let second_id = second.id();
    assert_eq!(second_id, TransactionId::new(0));
    assert!(!journal.is_committed(second_id));
    second.stop().unwrap();
    let second = journal.force_commit(second_id).unwrap();
    journal.start_checkpoint_for_test(&second).unwrap();
    assert!(journal.is_committed(first_id));
    assert!(journal.is_committed(second_id));
    journal.finish_checkpoint_for_test(&first).unwrap();
    journal.finish_checkpoint_for_test(&second).unwrap();
    let third = journal.begin(JournalCredits::new(0)).unwrap();
    let third_id = third.id();
    third.stop().unwrap();
    journal.force_commit(third_id).unwrap();
    journal.abort(Ext4Error::NoSpace);
    assert!(journal.is_committed(first_id));
    assert!(journal.is_committed(second_id));
    assert!(!journal.is_committed(third_id));
}

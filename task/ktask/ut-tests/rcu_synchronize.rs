// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The boot service intentionally persists for the entire kernel test run.
//! Local completion fixtures never replace its action, nodes or driver.

use unittest::{assert, assert_eq, def_test};

use super::*;

#[def_test]
fn masked_context_is_rejected_before_submission() {
    let result = {
        let _irq = kspin::IrqSave::new();
        PreparedGracePeriodWait::prepare()
    };
    assert!(matches!(result, Err(KError::InvalidInput)));
}

#[def_test]
fn local_read_scope_is_rejected_before_submission() {
    let current = crate::current();
    let was_rejected = current.rcu_reader.with_read(|_| {
        matches!(PreparedGracePeriodWait::prepare(), Err(KError::InvalidInput))
    });
    assert!(was_rejected);
}

#[def_test]
fn wake_hint_does_not_complete_a_grace_period() {
    let mut prepared = PreparedGracePeriodWait::prepare().unwrap();
    prepared.wait.waker().wake();
    // Consume a retained wake without a callback. The completion predicate,
    // not a return from park, is the authority used by submit_and_wait.
    prepared.wait.park();
    assert!(!prepared.completion.completion.try_wait());
}

#[def_test]
fn abandoned_preparation_unregisters_without_submitting_callback() {
    let prepared = PreparedGracePeriodWait::prepare().unwrap();
    let record = prepared.completion.clone();
    drop(prepared);
    // No raw callback owner was transferred. Keeping only the local record
    // lets us check cancellation without observing the global service queue.
    assert_eq!(Arc::strong_count(&record), 1);
    assert_eq!(record.completion.complete(), 0);
    assert!(record.completion.try_wait());
}

#[def_test]
fn completion_before_park_retains_its_result() {
    let prepared = PreparedGracePeriodWait::prepare().unwrap();
    let record = Arc::into_raw(prepared.completion.clone());
    // SAFETY: raw projection preserves the independently retained Arc owner.
    let node = unsafe { &raw const (*record).node }.cast_mut();
    // SAFETY: this local fixture has never been queued or published to readers;
    // it directly grants the callback the sole raw Arc reference, without locks.
    unsafe { complete_grace_period(node) };
    assert!(prepared.completion.completion.try_wait());
    assert!(!prepared.completion.completion.try_wait());
    assert_eq!(Arc::strong_count(&prepared.completion), 1);
}

#[def_test]
fn notifier_ownership_survives_waiter_destruction() {
    let prepared = PreparedGracePeriodWait::prepare().unwrap();
    let observer = Arc::downgrade(&prepared.completion);
    let notifier = prepared.completion.clone();
    notifier.completion.complete();
    assert!(prepared.completion.completion.try_wait());
    drop(prepared);
    // Model the waiter returning while complete_grace_period still owns its
    // Arc. The notifier may finish its remaining accesses independently.
    assert_eq!(Arc::strong_count(&notifier), 1);
    assert!(!notifier.completion.is_completed());
    drop(notifier);
    assert!(observer.upgrade().is_none());
}

#[def_test(serial)]
fn synchronous_wait_uses_the_live_callback_service() {
    for _ in 0..4 {
        // SAFETY: no object is retired by this fixture and no read scope or
        // lock is held. Boot established the permanent participating services.
        assert_eq!(unsafe { synchronize_rcu() }, Ok(()));
    }
}

#[def_test(serial)]
fn pending_interrupt_does_not_cancel_synchronous_wait() {
    let current = crate::current();
    let was_interrupted = current.is_interrupted();
    current.interrupt();
    // SAFETY: this task-only fixture retires no object and holds no read scope
    // or lock. Boot retains the participating services throughout the wait.
    let result = unsafe { synchronize_rcu() };
    let is_still_interrupted = current.is_interrupted();
    if !was_interrupted {
        current.clear_interrupt();
    }
    assert_eq!(result, Ok(()));
    assert!(is_still_interrupted);
}

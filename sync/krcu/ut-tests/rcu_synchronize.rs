// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The boot service intentionally persists for the entire kernel test run.
//! Local completion fixtures never replace its action, nodes or driver.

use unittest::{assert, assert_eq, def_test};

use super::*;

#[def_test]
fn masked_context_is_rejected_before_submission() {
    let mut was_called = false;
    let result = {
        let _irq = kspin::IrqSave::new();
        with_prepared_grace_period(|_| was_called = true)
    };
    assert!(matches!(result, Err(KError::InvalidInput)));
    assert!(!was_called);
}

#[def_test]
fn local_read_scope_is_rejected_before_submission() {
    let was_rejected = crate::host::with_current_reader(|current| {
        current.reader().with_read(|_| {
            matches!(
                with_prepared_grace_period(|_| ()),
                Err(KError::InvalidInput)
            )
        })
    });
    assert!(was_rejected);
}

#[def_test]
fn wake_hint_does_not_complete_a_grace_period() {
    let is_complete = with_prepared_grace_period(|prepared| {
        prepared.wait.waker().wake();
        // Consume a retained wake without a callback. The completion predicate,
        // not a return from park, is the authority used by submit_and_wait.
        prepared.wait.park();
        prepared.completion.completion.try_wait()
    })
    .unwrap();
    assert!(!is_complete);
}

#[def_test]
fn abandoned_preparation_unregisters_without_submitting_callback() {
    let record = with_prepared_grace_period(|prepared| prepared.completion.clone()).unwrap();
    // No raw callback owner was transferred. Keeping only the local record
    // lets us check cancellation without observing the global service queue.
    assert_eq!(Arc::strong_count(&record), 1);
    assert_eq!(record.completion.complete(), 0);
    assert!(record.completion.try_wait());
}

#[def_test]
fn completion_before_park_retains_its_result() {
    let (first, second, owners) = with_prepared_grace_period(|prepared| {
        let record = Arc::into_raw(prepared.completion.clone());
        // SAFETY: raw projection preserves the independently retained Arc owner.
        let node = unsafe { &raw const (*record).node }.cast_mut();
        // SAFETY: this fixture was never queued or published to readers; it
        // grants the callback the sole raw Arc reference without locks held.
        unsafe { complete_grace_period(node) };
        (
            prepared.completion.completion.try_wait(),
            prepared.completion.completion.try_wait(),
            Arc::strong_count(&prepared.completion),
        )
    })
    .unwrap();
    assert!(first);
    assert!(!second);
    // The unsubmitted handle still retains its independent cancellation owner.
    assert_eq!(owners, 2);
}

#[def_test]
fn notifier_ownership_survives_waiter_destruction() {
    let (notifier, was_complete) = with_prepared_grace_period(|prepared| {
        let notifier = prepared.completion.clone();
        notifier.completion.complete();
        let was_complete = prepared.completion.completion.try_wait();
        (notifier, was_complete)
    })
    .unwrap();
    let observer = Arc::downgrade(&notifier);
    assert!(was_complete);
    // Model the waiter returning while complete_grace_period still owns its
    // Arc. The notifier may finish its remaining accesses independently.
    assert_eq!(Arc::strong_count(&notifier), 1);
    assert!(!notifier.completion.is_completed());
    drop(notifier);
    assert!(observer.upgrade().is_none());
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, sync::Arc, vec};
use core::{assert, assert_eq, num::NonZeroUsize, pin::pin};

use unittest::def_test;

// Terminal kernel assertions cannot return early and release Work still borrowed
// by a partition; successful paths explicitly drain every accepted occurrence.
use super::QueueInFlight;
use crate::{
    queue::partition::QueuePartition,
    work::{Work, observers::CompletionObserver},
};

/// Checks colors are independent and only the last contribution completes observers.
///
/// # Panics
/// Panics on early completion or cross-color interference.
#[def_test]
fn colors_complete_independently() {
    let counts = QueueInFlight::new();
    assert!(counts.add(0).is_ok());
    assert!(counts.add(0).is_ok());
    assert!(counts.add(1).is_ok());
    let (first, token) = CompletionObserver::prepare();
    assert!(counts.observe(0, token).is_ok());
    let (second, token) = CompletionObserver::prepare();
    assert!(counts.observe(1, token).is_ok());
    counts.retire_after_cleanup(0).complete();
    assert!(!first.is_completed());
    counts.retire_after_cleanup(1).complete();
    assert!(second.is_completed());
    assert!(!first.is_completed());
    let detached = counts.retire_after_cleanup(0);
    assert!(!first.is_completed());
    detached.complete();
    assert!(first.is_completed());
}

struct Resource(Option<Box<dyn FnOnce() + Send>>);

impl Drop for Resource {
    fn drop(&mut self) {
        if let Some(callback) = self.0.take() {
            callback();
        }
    }
}

/// Checks a late queue observer still sees cleanup excluded by late Work capture.
///
/// # Panics
/// Panics if cancellation loses old accounting or cleanup touches replacement Work.
#[def_test]
fn canceled_cleanup_remains_in_original_queue() {
    let work = Arc::new(Work::new());
    let old_queue = Arc::new(QueuePartition::new(
        kcpu_id_map::LogicalCpuId::new(0),
        NonZeroUsize::new(1).unwrap(),
    ));
    let new_queue = Arc::new(QueuePartition::new(
        kcpu_id_map::LogicalCpuId::new(0),
        NonZeroUsize::new(1).unwrap(),
    ));
    let replacement_control = new_queue.clone();
    let weak = Arc::downgrade(&work);
    let old_control = old_queue.clone();
    let (receipt, token) = CompletionObserver::prepare();
    let during_cleanup = receipt.clone();
    let resource = Resource(Some(Box::new(move || {
        let work = weak.upgrade().unwrap();
        core::assert_eq!(work.capture(&mut vec![]), Ok(0));
        core::assert!(old_control.observe_batch(0, token).is_ok());
        core::assert!(!during_cleanup.is_completed());
        // SAFETY: outer Arc retains stable Work through replacement cancellation.
        core::assert!(unsafe { replacement_control.accept(&work, Resource(None), 1) }.is_ok());
    })));
    // SAFETY: outer Arc keeps Work stable and alive through both cancellations.
    assert!(unsafe { old_queue.accept(&work, resource, 0) }.is_ok());
    assert!(work.cancel_pending());
    assert!(receipt.is_completed());
    assert_eq!(work.observer_count(), 1);
    assert!(work.cancel_pending());
    let (_, token) = CompletionObserver::prepare();
    assert!(new_queue.observe_batch(1, token).is_err());
}

/// Checks empty capture preserves its token and completed receipts survive local reuse.
///
/// # Panics
/// Panics if an empty capture consumes its token or reuse resets an old receipt.
#[def_test]
fn empty_capture_and_reuse_preserve_receipts() {
    let counts = QueueInFlight::new();
    let (old, token) = CompletionObserver::prepare();
    let token = counts.observe(0, token).err().unwrap();
    assert!(!old.is_completed());
    assert!(counts.add(0).is_ok());
    assert!(counts.observe(0, token).is_ok());
    counts.retire_after_cleanup(0).complete();
    assert!(old.is_completed());
    assert!(counts.add(0).is_ok());
    let (new, token) = CompletionObserver::prepare();
    assert!(counts.observe(0, token).is_ok());
    assert!(old.is_completed());
    assert!(!new.is_completed());
    counts.retire_after_cleanup(0).complete();
    assert!(new.is_completed());
}

/// Checks representational overflow preserves the count without wrapping to zero.
///
/// # Panics
/// Panics if overflow succeeds or changes the existing count.
#[def_test]
fn count_overflow_has_no_effect() {
    let counts = QueueInFlight::new();
    counts.in_flight.lock()[0].outstanding = usize::MAX;
    assert!(counts.add(0).is_err());
    assert_eq!(counts.in_flight.lock()[0].outstanding, usize::MAX);
}

/// Checks failed Work admission neither charges a color nor consumes resources.
///
/// # Panics
/// Panics if duplicate admission changes accounting or destroys rejected resources.
#[def_test]
fn rejected_admission_keeps_accounting_unchanged() {
    let control = Arc::new(QueuePartition::new(
        kcpu_id_map::LogicalCpuId::new(0),
        NonZeroUsize::new(1).unwrap(),
    ));
    let work = pin!(Work::new());
    // SAFETY: pinned Work stays live and shared until cancellation below.
    assert!(unsafe { control.accept(&work, 1, 0) }.is_ok());
    // SAFETY: the same retained pinned storage is used and drained below.
    assert_eq!(unsafe { control.accept(&work, 2, 1) }, Err(2));
    let (_, token) = CompletionObserver::prepare();
    assert!(control.observe_batch(1, token).is_err());
    let (old, token) = CompletionObserver::prepare();
    assert!(control.observe_batch(0, token).is_ok());
    assert!(work.cancel_pending());
    assert!(old.is_completed());
}

impl QueueInFlight {
    /// Builds a test-only exhausted batch without exposing counter fields to Work.
    pub(in crate::queue::partition) fn exhausted_for_test() -> Self {
        let counts = Self::new();
        counts.in_flight.lock()[1].outstanding = usize::MAX;
        counts
    }
}

/// Checks accepted Work retains its control until cleanup finishes.
///
/// # Panics
/// Panics if control storage is freed early or retained after completed cancellation.
#[def_test]
fn accepted_work_retains_control_through_cleanup() {
    let control = Arc::new(QueuePartition::new(
        kcpu_id_map::LogicalCpuId::new(0),
        NonZeroUsize::new(1).unwrap(),
    ));
    let weak = Arc::downgrade(&control);
    let cleanup_control = weak.clone();
    let work = pin!(Work::new());
    let resource = Resource(Some(Box::new(move || {
        core::assert!(cleanup_control.upgrade().is_some());
    })));
    // SAFETY: pinned Work is retained until synchronous cancellation completes.
    assert!(unsafe { control.accept(&work, resource, 0) }.is_ok());
    let (receipt, token) = CompletionObserver::prepare();
    assert!(control.observe_batch(0, token).is_ok());
    drop(control);
    assert!(weak.upgrade().is_some());
    assert!(work.cancel_pending());
    assert!(receipt.is_completed());
    assert!(weak.upgrade().is_none());
}

struct RunSuccessor {
    work: alloc::sync::Weak<Work<Resource>>,
}

impl alloc::task::Wake for RunSuccessor {
    fn wake(self: Arc<Self>) {
        self.work
            .upgrade()
            .unwrap()
            .claim()
            .expect("running retires before queue notification")
            .run();
    }
}

/// Checks queue notification follows both destructor completion and running retirement.
///
/// # Panics
/// Panics on early completion, locked wake reentry or failure to run the successor.
#[def_test]
fn execution_retires_before_queue_notification() {
    let work = Arc::new(Work::new());
    let control = Arc::new(QueuePartition::new(
        kcpu_id_map::LogicalCpuId::new(0),
        NonZeroUsize::new(1).unwrap(),
    ));
    let (receipt, token) = CompletionObserver::prepare();
    let during_cleanup = receipt.clone();
    let resource = Resource(Some(Box::new(move || {
        core::assert!(!during_cleanup.is_completed());
    })));
    // SAFETY: outer Arc keeps Work stable through both executions and wake reentry.
    assert!(unsafe { control.accept(&work, resource, 0) }.is_ok());
    assert!(control.observe_batch(0, token).is_ok());
    let wake = Arc::new(RunSuccessor {
        work: Arc::downgrade(&work),
    });
    let registration = receipt.register(&core::task::Waker::from(wake)).unwrap();
    let execution = work.claim().unwrap();
    // SAFETY: successor finishes in wake reentry before the outer owner is dropped.
    assert!(unsafe { control.accept(&work, Resource(None), 1) }.is_ok());
    execution.run_with(|| core::assert!(!receipt.is_completed()));
    assert!(receipt.is_completed());
    assert_eq!(work.observer_count(), 0);
    let (_, token) = CompletionObserver::prepare();
    assert!(control.observe_batch(1, token).is_err());
    drop(registration);
}

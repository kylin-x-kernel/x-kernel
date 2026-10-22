// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
    task::Wake,
    vec,
};
use core::{
    num::NonZeroUsize,
    pin::{Pin, pin},
    ptr::NonNull,
    sync::atomic::{AtomicUsize, Ordering},
    task::Waker,
};

use kspin::SpinNoIrq;
use unittest::{assert, assert_eq, def_test};

use super::super::{Work, observers::CompletionObserver};
use crate::queue::partition::QueuePartition;

fn release_in_terminal_gap<H: Fn() + Send + Sync>(callback: H, is_partitioned: bool) {
    let partition = Arc::new(QueuePartition::new(
        kcpu_id_map::LogicalCpuId::new(0),
        NonZeroUsize::new(1).unwrap(),
    ));
    let work = Box::into_raw(Box::new(Work::with_callback(callback)));
    let (receipt, token) = CompletionObserver::prepare();
    let (queue_receipt, queue_token) = CompletionObserver::prepare();
    {
        // SAFETY: the raw Box uniquely retains this stable allocation until the
        // explicit destruction below, after terminal publication and all borrows.
        let borrowed = unsafe { &*work };
        if is_partitioned {
            // SAFETY: the Box remains live through admission and the unique
            // execution's shared cleanup; no other producer can submit it.
            core::assert!(unsafe { partition.accept(borrowed, (), 0) }.is_ok());
            core::assert!(partition.observe_batch(0, queue_token).is_ok());
        } else {
            core::assert!(borrowed.accept(()).is_ok());
        }
        core::assert_eq!(borrowed.capture(&mut vec![token]), Ok(1));
    }
    let claimed = if is_partitioned {
        let super::Execution { work, .. } = partition.claim_next().unwrap();
        work
    } else {
        // SAFETY: the unpartitioned fixture's sole Box owner keeps this state
        // initialized; claim establishes the same unique running responsibility.
        core::assert!(unsafe { &*work }.state.lock().claim());
        NonNull::new(work).unwrap().cast()
    };
    let (running_partition, retirement) = {
        // SAFETY: this fixture consumed the sole execution claim and retains
        // storage until its shared callback and cleanup have returned.
        let borrowed = unsafe { &*work };
        (borrowed.callback)();
        borrowed.prepare_completion()
    };
    core::assert!(retirement.is_none());
    // SAFETY: callback and shared cleanup returned with running still occupied;
    // claimed preserves the original Work type and running partition ownership.
    let finished = unsafe { Work::<(), H>::finish_execution(claimed, running_partition) };
    core::assert!(!receipt.is_completed());
    {
        // SAFETY: the raw Box is still retained. This new caller arrives after
        // idle publication, with no earlier shared Work call on the stack.
        let borrowed = unsafe { &*work };
        let (_, late_token) = CompletionObserver::prepare();
        core::assert_eq!(borrowed.capture(&mut vec![late_token]), Ok(0));
        core::assert!(!borrowed.cancel_sync().unwrap());
    }
    // SAFETY: cancellation observed no pending/running work, every shared call
    // returned, and there is no producer or retirement hook retaining this Box.
    drop(unsafe { Box::from_raw(work) });
    Work::<(), H>::complete_execution(finished);
    core::assert!(receipt.is_completed());
    if is_partitioned {
        core::assert!(queue_receipt.is_completed());
        let (_, empty_token) = CompletionObserver::prepare();
        core::assert!(partition.observe_batch(0, empty_token).is_err());
    }
}

/// A late cancellation may destroy Work before old completion notifications.
///
/// # Panics
/// Panics on early notification, stale running state or duplicate destruction.
#[def_test]
fn late_cancel_releases_work_before_old_notifications() {
    for is_partitioned in [false, true] {
        let calls = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let capture = CallbackCapture {
            calls: calls.clone(),
            drops: drops.clone(),
        };
        release_in_terminal_gap(move || capture.call(), is_partitioned);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }
}

struct Resource {
    on_drop: Option<Box<dyn FnOnce() + Send>>,
    drops: Arc<AtomicUsize>,
}

impl Drop for Resource {
    fn drop(&mut self) {
        if let Some(on_drop) = self.on_drop.take() {
            on_drop();
        }
        self.drops.fetch_add(1, Ordering::Relaxed);
    }
}

/// Checks claim precedes callback entry and cannot be revoked by pending cancellation.
///
/// # Panics
/// Panics if cancellation revokes a claimed occurrence or a second claim succeeds.
#[def_test]
fn claimed_execution_is_exclusive_before_callback_entry() {
    let work = Work::new();
    assert!(work.claim().is_none());
    assert!(work.accept(1).is_ok());
    let execution = work.claim().unwrap();
    assert!(!work.cancel_pending());
    assert!(work.claim().is_none());
    let (receipt, token) = CompletionObserver::prepare();
    assert_eq!(work.capture(&mut vec![token]), Ok(1));
    execution.run_with(|| {
        core::assert!(!receipt.is_completed());
        core::assert!(work.claim().is_none());
    });
    assert!(receipt.is_completed());
    assert_eq!(work.observer_count(), 0);
}

/// Checks callback/destructor reentry never permits the successor before cleanup.
///
/// # Panics
/// Panics on early successor claim, early completion or incorrect resource release.
#[def_test]
fn successor_waits_through_callback_and_destructor() {
    let work = Arc::new(Work::new());
    let weak = Arc::downgrade(&work);
    let drops = Arc::new(AtomicUsize::new(0));
    let (old, old_token) = CompletionObserver::prepare();
    let (during_cleanup, token) = CompletionObserver::prepare();
    let mut prepared = vec![token];
    let destructor_old = old.clone();
    assert!(
        work.accept(Resource {
            drops: drops.clone(),
            on_drop: Some(Box::new(move || {
                let work = weak.upgrade().unwrap();
                core::assert!(work.claim().is_none());
                core::assert!(!destructor_old.is_completed());
                // Withdraw the successor so capture covers only running cleanup.
                core::assert!(work.cancel_pending());
                core::assert_eq!(work.capture(&mut prepared), Ok(1));
            })),
        })
        .is_ok()
    );
    assert_eq!(work.capture(&mut vec![old_token]), Ok(1));
    let execution = work.claim().unwrap();
    execution.run_with(|| {
        core::assert!(
            work.accept(Resource {
                on_drop: None,
                drops: drops.clone()
            })
            .is_ok()
        );
        core::assert!(work.claim().is_none());
        core::assert!(!old.is_completed());
    });
    assert!(old.is_completed());
    assert!(during_cleanup.is_completed());
    assert_eq!(drops.load(Ordering::Relaxed), 2);
    assert!(
        work.accept(Resource {
            on_drop: None,
            drops: drops.clone()
        })
        .is_ok()
    );
    work.claim().unwrap().run();
    assert_eq!(drops.load(Ordering::Relaxed), 3);
}

struct RunSuccessor {
    work: Weak<Work<u8>>,
    runs: AtomicUsize,
}

impl Wake for RunSuccessor {
    fn wake(self: Arc<Self>) {
        let work = self.work.upgrade().unwrap();
        let successor = work.claim().expect("cleanup retired before notification");
        successor.run_with(|| {
            self.runs.fetch_add(1, Ordering::Relaxed);
        });
    }
}

/// Checks wake reentry can execute a successor after predecessor cleanup retires.
///
/// # Panics
/// Panics if wake runs locked, sees occupied running storage or completes the wrong scope.
#[def_test]
fn terminal_wake_can_run_successor() {
    let work = Arc::new(Work::new());
    let wake = Arc::new(RunSuccessor {
        work: Arc::downgrade(&work),
        runs: AtomicUsize::new(0),
    });
    let (old, token) = CompletionObserver::prepare();
    let registration = old.register(&Waker::from(wake.clone())).unwrap();
    assert!(work.accept(1).is_ok());
    assert_eq!(work.capture(&mut vec![token]), Ok(1));
    let execution = work.claim().unwrap();
    assert!(work.accept(2).is_ok());
    execution.run();
    assert!(old.is_completed());
    assert_eq!(wake.runs.load(Ordering::Relaxed), 1);
    assert_eq!(work.observer_count(), 0);
    drop(registration);
}

/// Checks pending cancellation from a callback cannot complete the running receipt.
///
/// # Panics
/// Panics if successor cancellation retires the active execution or its observer.
#[def_test]
fn callback_cancellation_preserves_running_observers() {
    let work = Work::new();
    assert!(work.accept(1).is_ok());
    let execution = work.claim().unwrap();
    let (running, token) = CompletionObserver::prepare();
    assert_eq!(work.capture(&mut vec![token]), Ok(1));
    execution.run_with(|| {
        core::assert!(work.accept(2).is_ok());
        core::assert!(work.cancel_pending());
        core::assert!(!running.is_completed());
        core::assert!(work.claim().is_none());
    });
    assert!(running.is_completed());
}

struct CallbackCapture {
    calls: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
}

impl CallbackCapture {
    fn call(&self) {
        self.calls.fetch_add(1, Ordering::Relaxed);
    }
}

impl Drop for CallbackCapture {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::Relaxed);
    }
}

/// Checks repeated execution and cancellation never clone or destroy bound captures.
///
/// # Panics
/// Panics on wrong callback count, early capture destruction or lost occurrence cleanup.
#[def_test]
fn bound_callback_lives_with_work_not_occurrences() {
    let calls = Arc::new(AtomicUsize::new(0));
    let capture_drops = Arc::new(AtomicUsize::new(0));
    let resource_drops = Arc::new(AtomicUsize::new(0));
    let partition = Arc::new(QueuePartition::new(
        kcpu_id_map::LogicalCpuId::new(0),
        NonZeroUsize::new(1).unwrap(),
    ));
    {
        let capture = CallbackCapture {
            calls: calls.clone(),
            drops: capture_drops.clone(),
        };
        let work = pin!(Work::with_callback(move || capture.call()));
        for i in 0..3 {
            let resource = Resource {
                on_drop: None,
                drops: resource_drops.clone(),
            };
            // SAFETY: pinned Work and captures outlive each fully retired execution.
            core::assert!(unsafe { partition.accept(&work, resource, i % 2) }.is_ok());
            let (receipt, token) = CompletionObserver::prepare();
            core::assert_eq!(work.capture(&mut vec![token]), Ok(1));
            partition.claim_next().unwrap().run();
            core::assert!(receipt.is_completed());
            core::assert_eq!(calls.load(Ordering::Relaxed), i + 1);
            core::assert_eq!(resource_drops.load(Ordering::Relaxed), i + 1);
            core::assert_eq!(capture_drops.load(Ordering::Relaxed), 0);
        }
        let resource = Resource {
            on_drop: None,
            drops: resource_drops.clone(),
        };
        // SAFETY: cancellation below drains pending before pinned Work is released.
        core::assert!(unsafe { partition.accept(&work, resource, 0) }.is_ok());
        let rejected = Resource {
            on_drop: None,
            drops: resource_drops.clone(),
        };
        // SAFETY: the same stable storage remains alive through final cancellation.
        let rejected = unsafe { partition.accept(&work, rejected, 1) }
            .err()
            .unwrap();
        core::assert_eq!(resource_drops.load(Ordering::Relaxed), 3);
        drop(rejected);
        core::assert!(work.cancel_pending());
        core::assert_eq!(calls.load(Ordering::Relaxed), 3);
        core::assert_eq!(resource_drops.load(Ordering::Relaxed), 5);
        core::assert_eq!(capture_drops.load(Ordering::Relaxed), 0);
    }
    core::assert_eq!(capture_drops.load(Ordering::Relaxed), 1);
}

/// Checks bound callbacks run unlocked and may submit and execute other prepared Work.
///
/// # Panics
/// Panics on early completion or lost nested execution; held management locks hang.
#[def_test]
fn bound_callback_reenters_partition_unlocked() {
    let partition = Arc::new(QueuePartition::new(
        kcpu_id_map::LogicalCpuId::new(0),
        NonZeroUsize::new(2).unwrap(),
    ));
    let nested_calls = AtomicUsize::new(0);
    let other = pin!(Work::with_callback(|| {
        nested_calls.fetch_add(1, Ordering::Relaxed);
    }));
    let (receipt, token) = CompletionObserver::prepare();
    let work = pin!(Work::with_callback(|| {
        core::assert!(!receipt.is_completed());
        // SAFETY: outer scope keeps other pinned and live through this nested run.
        core::assert!(unsafe { partition.accept(&other, 7, 0) }.is_ok());
        partition.claim_next().unwrap().run();
        core::assert!(!receipt.is_completed());
    }));
    // SAFETY: both externally pinned objects and all captures outlive the run below.
    core::assert!(unsafe { partition.accept(&work, 1, 0) }.is_ok());
    core::assert_eq!(work.capture(&mut vec![token]), Ok(1));
    partition.claim_next().unwrap().run();
    core::assert_eq!(nested_calls.load(Ordering::Relaxed), 1);
    core::assert!(receipt.is_completed());
    core::assert_eq!(other.observer_count(), 0);
    core::assert_eq!(work.observer_count(), 0);
}

struct ReleaseOnCompletion {
    work: SpinNoIrq<Option<Pin<Box<Work<()>>>>>,
    releases: AtomicUsize,
}

impl Wake for ReleaseOnCompletion {
    fn wake(self: Arc<Self>) {
        let work = self.work.lock().take().expect("one terminal release");
        drop(work);
        self.releases.fetch_add(1, Ordering::Relaxed);
    }
}

/// Checks both queue and Work completion may release already-drained external storage.
///
/// # Panics
/// Panics on early/duplicate release, stale completion or missing terminal notification.
#[def_test]
fn terminal_notification_can_release_work_storage() {
    for release_from_queue in [false, true] {
        let partition = Arc::new(QueuePartition::new(
            kcpu_id_map::LogicalCpuId::new(0),
            NonZeroUsize::new(1).unwrap(),
        ));
        let owner = Arc::new(ReleaseOnCompletion {
            work: SpinNoIrq::new(Some(Box::pin(Work::with_callback((|| {}) as fn())))),
            releases: AtomicUsize::new(0),
        });
        let (work_receipt, work_token) = CompletionObserver::prepare();
        let (queue_receipt, queue_token) = CompletionObserver::prepare();
        let release_receipt = if release_from_queue {
            &queue_receipt
        } else {
            &work_receipt
        };
        let registration = release_receipt
            .register(&Waker::from(owner.clone()))
            .unwrap();
        {
            let borrowed = owner.work.lock();
            let work = borrowed.as_ref().unwrap();
            // SAFETY: owner keeps this pinned allocation through terminal cleanup;
            // no producer, API borrow or successor remains when the waiter frees it.
            core::assert!(unsafe { partition.accept(work, (), 0) }.is_ok());
            core::assert_eq!(work.capture(&mut vec![work_token]), Ok(1));
            core::assert!(partition.observe_batch(0, queue_token).is_ok());
        }
        partition.claim_next().unwrap().run();
        core::assert!(work_receipt.is_completed());
        core::assert!(queue_receipt.is_completed());
        core::assert!(owner.work.lock().is_none());
        core::assert_eq!(owner.releases.load(Ordering::Relaxed), 1);
        core::assert!(partition.claim_next().is_none());
        drop(registration);
    }
}

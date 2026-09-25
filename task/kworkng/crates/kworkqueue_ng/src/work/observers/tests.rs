// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{sync::Arc, task::Wake};
use core::{
    sync::atomic::{AtomicUsize, Ordering},
    task::Waker,
};

use unittest::{assert, assert_eq, def_test};

use super::{CompletionObserver, CompletionObservers};

struct WakeCount(AtomicUsize);

impl Wake for WakeCount {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

/// Checks whole-chain ownership transfer and isolation from replacement work.
///
/// # Panics
/// Panics if transfer completes anything or old completion reaches the successor.
#[def_test]
fn transferred_chain_does_not_complete_replacement() {
    let mut pending = CompletionObservers::new();
    let observers: [_; 128] = core::array::from_fn(|_| {
        let (receipt, prepared) = CompletionObserver::prepare();
        pending.attach(prepared);
        receipt
    });
    let mut running = pending.take();
    let cleanup = running.take();
    let (next, prepared) = CompletionObserver::prepare();
    pending.attach(prepared);
    running.complete();
    assert!(observers.iter().all(|observer| !observer.is_completed()));
    assert!(!next.is_completed());
    cleanup.complete();
    assert!(observers.iter().all(|observer| observer.is_completed()));
    assert!(!next.is_completed());
    pending.complete();
    assert!(next.is_completed());
    assert!(observers.iter().all(|observer| observer.is_completed()));
}

/// Checks that the chain keeps an abandoned receipt alive only until completion.
///
/// # Panics
/// Panics if abandonment prematurely frees the node or terminal cleanup leaks it.
#[def_test]
fn abandoned_receipt_is_reclaimed_at_completion() {
    let (receipt, prepared) = CompletionObserver::prepare();
    let weak = Arc::downgrade(&receipt);
    let mut observers = CompletionObservers::new();
    observers.attach(prepared);
    drop(receipt);
    assert_eq!(weak.strong_count(), 1);
    observers.take().complete();
    assert!(weak.upgrade().is_none());
    observers.complete();
}

/// Checks no wake during attachment/transfer and permanent readiness after wake.
///
/// # Panics
/// Panics if registration fails or notification occurs before terminal completion.
#[def_test]
fn wake_is_deferred_until_detached_completion() {
    let counter = Arc::new(WakeCount(AtomicUsize::new(0)));
    let waker = Waker::from(counter.clone());
    let (receipt, prepared) = CompletionObserver::prepare();
    let registration = receipt.register(&waker).unwrap();
    let mut observers = CompletionObservers::new();
    observers.attach(prepared);
    let detached = observers.take();
    assert_eq!(counter.0.load(Ordering::Relaxed), 0);
    assert!(!receipt.is_completed());
    detached.complete();
    assert_eq!(counter.0.load(Ordering::Relaxed), 1);
    assert!(receipt.is_completed());
    assert!(receipt.is_completed());
    drop(registration);
}

/// Checks canceled preparation and discarded chains never report success.
///
/// # Panics
/// Panics if dropping uncompleted storage fabricates a completion result.
#[def_test]
fn discarded_storage_does_not_report_completion() {
    let (receipt, prepared) = CompletionObserver::prepare();
    drop(prepared);
    assert!(!receipt.is_completed());
    assert_eq!(Arc::strong_count(&receipt), 1);
    let (attached, prepared) = CompletionObserver::prepare();
    let mut observers = CompletionObservers::new();
    observers.attach(prepared);
    drop(observers);
    assert!(!attached.is_completed());
    assert_eq!(Arc::strong_count(&attached), 1);
}

/// Checks rechecking readiness covers completion before wake registration.
///
/// # Panics
/// Panics if late registration fails or hides permanent completion.
#[def_test]
fn completion_before_registration_remains_observable() {
    let counter = Arc::new(WakeCount(AtomicUsize::new(0)));
    let waker = Waker::from(counter.clone());
    let (receipt, prepared) = CompletionObserver::prepare();
    let mut observers = CompletionObservers::new();
    observers.attach(prepared);
    observers.complete();
    let registration = receipt.register(&waker).unwrap();
    assert!(receipt.is_completed());
    assert_eq!(counter.0.load(Ordering::Relaxed), 0);
    drop(registration);
}

/// Checks canceled wake registration does not affect subject completion.
///
/// # Panics
/// Panics if unregistering wakes the observer or prevents terminal readiness.
#[def_test]
fn canceled_wake_registration_does_not_cancel_observation() {
    let counter = Arc::new(WakeCount(AtomicUsize::new(0)));
    let waker = Waker::from(counter.clone());
    let (receipt, prepared) = CompletionObserver::prepare();
    let registration = receipt.register(&waker).unwrap();
    let mut observers = CompletionObservers::new();
    observers.attach(prepared);
    drop(registration);
    observers.complete();
    assert!(receipt.is_completed());
    assert_eq!(counter.0.load(Ordering::Relaxed), 0);
}

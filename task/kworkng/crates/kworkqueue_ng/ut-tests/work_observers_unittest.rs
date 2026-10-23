// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use ktask::completion::CompletionWait;
use unittest::{assert, assert_eq, def_test};

use super::{CompletionObserver, CompletionObservers};

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

/// Attachment and transfer do not release a sleeping task before completion.
#[def_test]
fn wake_is_deferred_until_detached_completion() {
    let (receipt, prepared) = CompletionObserver::prepare();
    let waiter = ktask::spawn({
        let receipt = receipt.clone();
        move || receipt.wait(&CompletionWait::prepare().unwrap())
    });
    while waiter.state() != ktask::TaskState::Blocked {
        ktask::yield_now();
    }
    let mut observers = CompletionObservers::new();
    observers.attach(prepared);
    let detached = observers.take();
    assert!(!receipt.is_completed());
    assert_eq!(waiter.state(), ktask::TaskState::Blocked);
    detached.complete();
    assert_eq!(waiter.join(), 0);
    assert!(receipt.is_completed());
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

/// Permanent completion supports late and repeated sequential waits.
#[def_test]
fn completion_before_wait_remains_observable() {
    let (receipt, prepared) = CompletionObserver::prepare();
    let mut observers = CompletionObservers::new();
    observers.attach(prepared);
    observers.complete();
    let wait = CompletionWait::prepare().unwrap();
    receipt.wait(&wait);
    receipt.wait(&wait);
    assert!(receipt.is_completed());
}

/// Independent tasks observe the same permanent receipt.
#[def_test]
fn concurrent_waiters_share_terminal_observation() {
    let (receipt, prepared) = CompletionObserver::prepare();
    let waiters: [_; 2] = core::array::from_fn(|_| {
        let receipt = receipt.clone();
        ktask::spawn(move || receipt.wait(&CompletionWait::prepare().unwrap()))
    });
    for waiter in &waiters {
        while waiter.state() != ktask::TaskState::Blocked {
            ktask::yield_now();
        }
    }
    let mut observers = CompletionObservers::new();
    observers.attach(prepared);
    observers.complete();
    for waiter in waiters {
        assert_eq!(waiter.join(), 0);
    }
    assert!(receipt.is_completed());
}

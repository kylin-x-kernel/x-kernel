// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, sync::Arc, task::Wake, vec, vec::Vec};
use core::sync::atomic::{AtomicUsize, Ordering};

use unittest::{assert, assert_eq, def_test};

use super::super::{Work, observers::CompletionObserver};

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

/// Checks old observers wait for destruction while new captures exclude that tail.
///
/// # Panics
/// Panics if cleanup is discovered by a later capture or an old receipt completes early.
#[def_test]
fn detached_cleanup_preserves_old_observers_only() {
    let work = Arc::new(Work::new());
    let weak = Arc::downgrade(&work);
    let drops = Arc::new(AtomicUsize::new(0));
    let callback_drops = drops.clone();
    let (old, old_token) = CompletionObserver::prepare();
    let callback_old = old.clone();
    let (new, new_token) = CompletionObserver::prepare();
    let mut new_tokens = vec![new_token];
    assert!(
        work.accept(Resource {
            drops: drops.clone(),
            on_drop: Some(Box::new(move || {
                let work = weak.upgrade().unwrap();
                core::assert_eq!(work.observer_count(), 0);
                core::assert_eq!(work.capture(&mut Vec::new()), Ok(0));
                core::assert!(!callback_old.is_completed());
                core::assert_eq!(callback_drops.load(Ordering::Relaxed), 0);
                core::assert!(
                    work.accept(Resource {
                        on_drop: None,
                        drops: callback_drops
                    })
                    .is_ok()
                );
                core::assert_eq!(work.capture(&mut new_tokens), Ok(1));
            })),
        })
        .is_ok()
    );
    assert_eq!(work.capture(&mut vec![old_token]), Ok(1));
    assert!(work.cancel_pending());
    assert!(old.is_completed());
    assert!(!new.is_completed());
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    assert!(work.cancel_pending());
    assert!(new.is_completed());
    assert_eq!(drops.load(Ordering::Relaxed), 2);
}

/// Checks nested cancellation needs neither a discoverable scope nor a spare slot.
///
/// # Panics
/// Panics if an old cleanup remains in capture or nested cancellation loses resources.
#[def_test]
fn nested_cancellation_keeps_cleanup_local() {
    let work = Arc::new(Work::new());
    let weak = Arc::downgrade(&work);
    let inner_weak = weak.clone();
    let drops = Arc::new(AtomicUsize::new(0));
    let inner = Resource {
        drops: drops.clone(),
        on_drop: Some(Box::new(move || {
            let work = inner_weak.upgrade().unwrap();
            core::assert_eq!(work.capture(&mut Vec::new()), Ok(0));
            core::assert!(!work.cancel_pending());
        })),
    };
    assert!(
        work.accept(Resource {
            drops: drops.clone(),
            on_drop: Some(Box::new(move || {
                let work = weak.upgrade().unwrap();
                core::assert!(work.accept(inner).is_ok());
                core::assert!(work.cancel_pending());
                core::assert_eq!(work.observer_count(), 0);
            })),
        })
        .is_ok()
    );
    assert!(work.cancel_pending());
    assert_eq!(drops.load(Ordering::Relaxed), 2);
}

/// Checks insufficient capture has no effects and retry captures both current slots.
///
/// # Panics
/// Panics if capture consumes insufficient preparation or confuses completion scopes.
#[def_test]
fn capture_rechecks_current_slots_without_partial_effects() {
    let work = Work::new();
    assert!(work.accept(1).is_ok());
    assert!(work.state.lock().claim());
    let (first, token) = CompletionObserver::prepare();
    let mut prepared = vec![token];
    assert_eq!(work.observer_count(), 1);
    assert!(work.accept(2).is_ok());
    assert_eq!(work.capture(&mut prepared), Err(2));
    assert_eq!(prepared.len(), 1);
    let (second, token) = CompletionObserver::prepare();
    prepared.push(token);
    assert_eq!(work.capture(&mut prepared), Ok(2));
    assert!(work.cancel_pending());
    assert!(second.is_completed());
    assert!(!first.is_completed());
    assert_eq!(work.state.lock().take_running_resources(), Some(1));
    let completed = work.state.lock().finish_after_cleanup().unwrap();
    completed.observers.complete();
    assert!(first.is_completed());
}

struct WakeCheck {
    work: alloc::sync::Weak<Work<u8>>,
    wakes: AtomicUsize,
}

impl Wake for WakeCheck {
    fn wake(self: Arc<Self>) {
        let work = self.work.upgrade().unwrap();
        core::assert_eq!(work.observer_count(), 0);
        core::assert!(!work.cancel_pending());
        self.wakes.fetch_add(1, Ordering::Relaxed);
    }
}

/// Checks completion wakeups can reenter after the final Work access and unlock.
///
/// # Panics
/// Panics if the resumed task cannot observe an empty Work.
#[def_test]
fn completion_wakes_outside_work_protection() {
    let work = Arc::new(Work::new());
    let check = Arc::new(WakeCheck {
        work: Arc::downgrade(&work),
        wakes: AtomicUsize::new(0),
    });
    let (receipt, token) = CompletionObserver::prepare();
    let waiter = ktask::spawn({
        let (receipt, owner) = (receipt.clone(), check.clone());
        move || {
            receipt.wait(&ktask::completion::CompletionWait::prepare().unwrap());
            owner.wake();
        }
    });
    while waiter.state() != ktask::TaskState::Blocked {
        ktask::yield_now();
    }
    assert!(work.accept(1).is_ok());
    assert_eq!(work.capture(&mut vec![token]), Ok(1));
    assert!(work.cancel_pending());
    assert!(receipt.is_completed());
    assert_eq!(waiter.join(), 0);
    assert_eq!(check.wakes.load(Ordering::Relaxed), 1);
}

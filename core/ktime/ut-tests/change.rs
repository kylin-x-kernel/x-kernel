// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

extern crate alloc;
use alloc::{sync::Arc, task::Wake};
use core::{
    sync::atomic::{AtomicUsize, Ordering},
    task::Waker,
};

use unittest::{assert, assert_eq, def_test};

use super::*;

struct Count(AtomicUsize);
impl Wake for Count {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}
fn count() -> (Arc<Count>, Waker) {
    let count = Arc::new(Count(AtomicUsize::new(0)));
    let waker = Waker::from(count.clone());
    (count, waker)
}
#[def_test]
fn wallclock_update_does_not_publish_boottime() {
    let changes = Changes::new();
    let boot = changes.boottime.generation();
    let real = changes.realtime.generation();
    wake(changes.publish(false));
    assert!(changes.realtime.has_changed_since(real));
    assert!(!changes.boottime.has_changed_since(boot));
}
#[def_test]
fn sleep_update_publishes_both_domains() {
    let changes = Changes::new();
    let boot = changes.boottime.generation();
    let real = changes.realtime.generation();
    wake(changes.publish(true));
    assert!(changes.realtime.has_changed_since(real));
    assert!(changes.boottime.has_changed_since(boot));
}
#[def_test]
fn change_before_first_poll_is_retained() {
    let changes = Changes::new();
    let observed = changes.realtime.generation();
    wake(changes.publish(false));
    let mut registrations = PollRegistrations::new();
    assert!(
        poll_change(
            &changes.realtime,
            observed,
            &mut registrations,
            &mut Context::from_waker(Waker::noop())
        )
        .is_ready()
    );
}
#[def_test]
fn publication_defers_callbacks_until_unlocked() {
    let changes = Changes::new();
    let observed = changes.realtime.generation();
    let mut registrations = PollRegistrations::new();
    let (count, waker) = count();
    let mut cx = Context::from_waker(&waker);
    assert!(poll_change(&changes.realtime, observed, &mut registrations, &mut cx).is_pending());
    let deferred = changes.publish(false);
    assert_eq!(count.0.load(Ordering::Relaxed), 0);
    assert!(changes.realtime.has_changed_since(observed));
    wake(deferred);
    assert_eq!(count.0.load(Ordering::Relaxed), 1);
    assert!(poll_change(&changes.realtime, observed, &mut registrations, &mut cx).is_ready());
}
#[def_test]
fn dropping_registration_removes_waker() {
    let changes = Changes::new();
    let mut registrations = PollRegistrations::new();
    let (count, waker) = count();
    assert!(
        poll_change(
            &changes.realtime,
            changes.realtime.generation(),
            &mut registrations,
            &mut Context::from_waker(&waker)
        )
        .is_pending()
    );
    drop(registrations);
    wake(changes.publish(false));
    assert_eq!(count.0.load(Ordering::Relaxed), 0);
    assert_eq!(Arc::strong_count(&count), 2);
}
#[def_test]
fn multiple_waiters_observe_one_publication() {
    let changes = Changes::new();
    let observed = changes.realtime.generation();
    let (first, a) = count();
    let (second, b) = count();
    let mut ra = PollRegistrations::new();
    let mut rb = PollRegistrations::new();
    assert!(
        poll_change(
            &changes.realtime,
            observed,
            &mut ra,
            &mut Context::from_waker(&a)
        )
        .is_pending()
    );
    assert!(
        poll_change(
            &changes.realtime,
            observed,
            &mut rb,
            &mut Context::from_waker(&b)
        )
        .is_pending()
    );
    wake(changes.publish(false));
    assert_eq!(first.0.load(Ordering::Relaxed), 1);
    assert_eq!(second.0.load(Ordering::Relaxed), 1);
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::sync::atomic::{AtomicUsize, Ordering};

use ktime_types::TimeSpan;
use unittest::{assert, assert_eq, def_test};

use super::*;
use crate::{Expiration, TimerControl};

struct Handler(Arc<AtomicUsize>);
impl TimerHandler for Handler {
    fn on_expiration(&self, _: TimerControl<'_>, _: Expiration) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}
impl Drop for Handler {
    fn drop(&mut self) {
        self.0.fetch_add(100, Ordering::Relaxed);
    }
}

fn deadline(ns: u64) -> MonotonicInstant {
    MonotonicInstant::from_span_since_origin(TimeSpan::from_nanos(ns))
}

#[def_test(serial)]
fn initial_owner_is_validated_and_does_not_pin_later_starts() {
    static TIMERS: TimerSet<2> = TimerSet::new();
    let hits = Arc::new(AtomicUsize::new(0));
    assert!(Timer::new_on(&TIMERS, 2.into(), Handler(hits.clone())).is_err());
    assert_eq!(hits.load(Ordering::Relaxed), 100);
    let timer = Timer::new_on(&TIMERS, 1.into(), Handler(hits.clone())).unwrap();
    assert_eq!(timer.data.node.owner_cpu(), 1.into());
    assert_eq!(timer.try_cancel(), CancelResult::Inactive);
    // SAFETY: owned storage, modeled CPUs and serialized dispatch; the
    // initial route is a hint, not a restriction on the first start.
    let first = unsafe { timer.start_on(0.into(), deadline(10)) }.unwrap();
    assert_eq!(first, (0.into(), true));
    // SAFETY: retained pending node can migrate to another modeled CPU.
    let replacement = unsafe { timer.start_on(1.into(), deadline(20)) }.unwrap();
    assert_eq!(replacement, (1.into(), false));
    assert!(timer.cancel_sync());
    drop(timer);
    let idle = Timer::new_on(&TIMERS, 1.into(), Handler(hits.clone())).unwrap();
    drop(idle);
    assert_eq!(hits.load(Ordering::Relaxed), 300);
    assert_eq!(Arc::strong_count(&hits), 1);
    // SAFETY: both modeled queues are drained; no callback can remain.
    assert!(!unsafe { TIMERS.dispatch_due(1.into(), || 30) }.unwrap());
}

#[def_test(serial)]
fn owned_try_cancel_distinguishes_idle_pending_and_reuse() {
    static TIMERS: TimerSet<1> = TimerSet::new();
    let hits = Arc::new(AtomicUsize::new(0));
    let timer = Timer::new(&TIMERS, Handler(hits.clone()));
    assert_eq!(timer.try_cancel(), CancelResult::Inactive);
    // SAFETY: isolated modeled CPU, live shell, serialized dispatch below.
    unsafe { timer.start_on(0.into(), deadline(10)) }.unwrap();
    assert_eq!(timer.try_cancel(), CancelResult::Removed);
    assert_eq!(timer.try_cancel(), CancelResult::Inactive);
    // SAFETY: sole modeled dispatcher, bounded monotonic clock, live shell.
    assert!(!unsafe { TIMERS.dispatch_due(0.into(), || 10) }.unwrap());
    assert_eq!(hits.load(Ordering::Relaxed), 0);
    // SAFETY: removal does not disable this live timer; same modeled CPU.
    unsafe { timer.start_on(0.into(), deadline(20)) }.unwrap();
    // SAFETY: same exclusive modeled dispatch and live callback storage.
    assert!(unsafe { TIMERS.dispatch_due(0.into(), || 20) }.unwrap());
    assert_eq!(timer.try_cancel(), CancelResult::Inactive);
    assert_eq!(hits.load(Ordering::Relaxed), 1);
}

#[def_test(serial)]
fn owned_timer_move_cancel_and_drop_preserve_storage() {
    static TIMERS: TimerSet<1> = TimerSet::new();
    let hits = Arc::new(AtomicUsize::new(0));
    let timer = Timer::new(&TIMERS, Handler(hits.clone()));
    // SAFETY: isolated static collection, stable owned storage, serialized
    // simulated CPU and atomic-only handler; no actual hardware is involved.
    unsafe { timer.start_on(0.into(), deadline(10)) }.unwrap();
    let moved = timer;
    // SAFETY: single modeled CPU service, fixed monotonic time, live shell.
    assert!(unsafe { TIMERS.dispatch_due(0.into(), || 10) }.unwrap());
    assert_eq!(hits.load(Ordering::Relaxed), 1);
    for _ in 0..32 {
        // SAFETY: same modeled service; this live shell owns all storage.
        unsafe { moved.start_on(0.into(), deadline(20)) }.unwrap();
        assert!(moved.cancel_sync());
    }
    drop(moved);
    assert_eq!(hits.load(Ordering::Relaxed), 101);
    assert_eq!(Arc::strong_count(&hits), 1);
}

#[def_test(serial)]
fn owned_wait_rejects_elapsed_without_replacing_pending_timer() {
    static TIMERS: TimerSet<1> = TimerSet::new();
    let hits = Arc::new(AtomicUsize::new(0));
    let timer = Timer::new(&TIMERS, Handler(hits.clone()));
    // SAFETY: isolated modeled CPU, stable owned handler, bounded clock.
    let accepted = unsafe { timer.start_on_if_future(0.into(), deadline(20), || 10) };
    assert!(accepted.unwrap().is_some());
    // SAFETY: same collection; equality must not replace its future entry.
    let rejected = unsafe { timer.start_on_if_future(0.into(), deadline(10), || 10) };
    assert!(rejected.unwrap().is_none());
    // SAFETY: modeled monotonic service while the owner remains live.
    assert!(!unsafe { TIMERS.dispatch_due(0.into(), || 15) }.unwrap());
    // SAFETY: same serialized service at the original pending deadline.
    assert!(unsafe { TIMERS.dispatch_due(0.into(), || 20) }.unwrap());
    assert_eq!(hits.load(Ordering::Relaxed), 1);
}

#[def_test(serial)]
fn owned_timer_drop_removes_pending_before_handler_destruction() {
    static TIMERS: TimerSet<1> = TimerSet::new();
    let hits = Arc::new(AtomicUsize::new(0));
    let timer = Timer::new(&TIMERS, Handler(hits.clone()));
    // SAFETY: modeled owner CPU, live storage; only this test services it.
    unsafe { timer.start_on(0.into(), deadline(10)) }.unwrap();
    // SAFETY: invalid target is rejected before modifying the pending timer.
    assert!(unsafe { timer.start_on(1.into(), deadline(20)) }.is_err());
    drop(timer);
    // SAFETY: cancellation has drained the only node; modeled clock is valid.
    assert!(!unsafe { TIMERS.dispatch_due(0.into(), || 30) }.unwrap());
    assert_eq!(hits.load(Ordering::Relaxed), 100);
    assert_eq!(Arc::strong_count(&hits), 1);
}

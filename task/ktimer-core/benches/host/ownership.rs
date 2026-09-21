// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Miri-compatible lifetime tests over the real core and host spinlock.

#![allow(dead_code)]
#[cfg(any(not(feature = "smp"), feature = "stats"))]
compile_error!("ownership tests require smp and do not support lock statistics");

extern crate alloc;
extern crate self as karch;
extern crate self as kcpu_id_map;
extern crate self as kspin;

#[path = "../../../../arch/karch/src/cache.rs"]
pub mod cache;
mod host;
pub use host::{BaseGuard, LogicalCpuId};
mod guard {
    pub use super::BaseGuard;
}
#[path = "../../../kspin/src/guard/types.rs"]
mod guard_types;
pub use guard_types::{IrqSave, NoOp, NoPreempt, NoPreemptIrqSave};
#[path = "../../../kspin/src/lock.rs"]
mod lock;
pub type SpinNoIrq<T> = lock::SpinLock<NoPreemptIrqSave, T>;
pub type SpinNoIrqGuard<'a, T> = lock::SpinLockGuard<'a, NoPreemptIrqSave, T>;
#[path = "../../src/control.rs"]
mod control;
#[path = "../../src/cpu.rs"]
mod cpu;
#[path = "../../src/deadline.rs"]
mod deadline;
#[path = "../../src/dispatch.rs"]
mod dispatch;
#[path = "../../src/index.rs"]
mod index;
#[path = "../../src/set.rs"]
mod set;
#[path = "../../src/timer.rs"]
mod timer;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};

pub use control::TimerControl;
pub use deadline::Expiration;
pub use index::TimerHandler;
pub use set::TimerSet;
pub use timer::{CancelResult, Timer};

mod ownership_races;

#[test]
fn explicit_initial_owner_allows_move_and_dispatch() {
    use ktime_types::{MonotonicInstant, TimeSpan};
    static TIMERS: TimerSet<2> = TimerSet::new();
    let timer = Timer::new_on(&TIMERS, 1.into(), |_: TimerControl<'_>, _: Expiration| {}).unwrap();
    let moved = timer;
    assert_eq!(moved.try_cancel(), CancelResult::Inactive);
    // SAFETY: live owned storage, serialized modeled CPUs and empty callback.
    unsafe {
        moved
            .start_on(
                0.into(),
                MonotonicInstant::from_span_since_origin(TimeSpan::from_nanos(10)),
            )
            .unwrap();
        assert!(TIMERS.dispatch_due(0.into(), || 10).unwrap());
    }
    assert!(!moved.cancel_sync());
    drop(moved);
    let idle = Timer::new_on(&TIMERS, 1.into(), |_: TimerControl<'_>, _: Expiration| {}).unwrap();
    drop(idle);
}

struct State {
    entered: AtomicBool,
    can_finish: AtomicBool,
    drops: AtomicUsize,
}

struct Handler(Arc<State>);
impl TimerHandler for Handler {
    fn on_expiration(&self, _: TimerControl<'_>, _: Expiration) {
        self.0.entered.store(true, Ordering::Release);
        while !self.0.can_finish.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
    }
}
impl Drop for Handler {
    fn drop(&mut self) {
        self.0.drops.fetch_add(1, Ordering::Release);
    }
}

#[test]
fn dropping_shell_waits_for_borrowed_handler() {
    static TIMERS: TimerSet<1> = TimerSet::new();
    let state = Arc::new(State {
        entered: AtomicBool::new(false),
        can_finish: AtomicBool::new(false),
        drops: AtomicUsize::new(0),
    });
    let timer = Timer::new(&TIMERS, Handler(state.clone()));
    let deadline = ktime_types::MonotonicInstant::from_span_since_origin(
        ktime_types::TimeSpan::from_nanos(10),
    );
    // SAFETY: one simulated CPU is serviced by the thread below. The static
    // set and owned allocation are stable; no hardware/IRQ behavior is modeled.
    unsafe { timer.start_on(0.into(), deadline) }.unwrap();
    let dispatch = std::thread::spawn(|| {
        // SAFETY: unique modeled dispatcher, valid monotonic clock. Timer Drop
        // retains node/handler storage until this delivery has fully retired.
        assert!(unsafe { TIMERS.dispatch_due(0.into(), || 10) }.unwrap());
    });
    while !state.entered.load(Ordering::Acquire) {
        std::thread::yield_now();
    }
    let dropping = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicBool::new(false));
    let cancel = {
        let dropping = dropping.clone();
        let dropped = dropped.clone();
        std::thread::spawn(move || {
            dropping.store(true, Ordering::Release);
            drop(timer);
            dropped.store(true, Ordering::Release);
        })
    };
    while !dropping.load(Ordering::Acquire) {
        std::thread::yield_now();
    }
    assert_eq!(state.drops.load(Ordering::Acquire), 0);
    assert!(!dropped.load(Ordering::Acquire));
    state.can_finish.store(true, Ordering::Release);
    dispatch.join().unwrap();
    cancel.join().unwrap();
    assert!(dropped.load(Ordering::Acquire));
    assert_eq!(state.drops.load(Ordering::Acquire), 1);
    assert_eq!(Arc::strong_count(&state), 1);
}

#[test]
fn nonwaiting_cancel_preserves_running_successor() {
    use ktime_types::{MonotonicInstant, TimeSpan};
    static TIMERS: TimerSet<1> = TimerSet::new();
    for has_successor in [false, true] {
        let state = Arc::new(State {
            entered: AtomicBool::new(false),
            can_finish: AtomicBool::new(false),
            drops: AtomicUsize::new(0),
        });
        let timer = Timer::new(&TIMERS, Handler(state.clone()));
        let deadline = |ns| MonotonicInstant::from_span_since_origin(TimeSpan::from_nanos(ns));
        // SAFETY: static collection, retained owner, one modeled dispatcher.
        unsafe { timer.start_on(0.into(), deadline(10)) }.unwrap();
        let dispatch = std::thread::spawn(|| {
            // SAFETY: exclusive modeled CPU service, bounded monotonic clock.
            unsafe { TIMERS.dispatch_due(0.into(), || 10) }.unwrap();
        });
        while !state.entered.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        if has_successor {
            // SAFETY: retained shell; owner routing serializes with live delivery.
            unsafe { timer.start_on(0.into(), deadline(30)) }.unwrap();
        }
        let result = timer.try_cancel();
        // Release before assertions so failures cannot strand the owned Drop.
        state.can_finish.store(true, Ordering::Release);
        dispatch.join().unwrap();
        assert_eq!(result, CancelResult::Running);
        // SAFETY: synchronous observer, no hardware, no nested timer operation.
        unsafe {
            TIMERS.reprogram_local_timer(0.into(), |next| {
                assert_eq!(next, has_successor.then_some(30));
            })
        }
        .unwrap();
        assert_eq!(
            timer.try_cancel(),
            if has_successor {
                CancelResult::Removed
            } else {
                CancelResult::Inactive
            }
        );
        assert_eq!(timer.try_cancel(), CancelResult::Inactive);
        assert_eq!(state.drops.load(Ordering::Acquire), 0);
        drop(timer);
        assert_eq!(state.drops.load(Ordering::Acquire), 1);
        assert_eq!(Arc::strong_count(&state), 1);
    }
}

#[test]
fn repeated_near_future_rearm_respects_backoff_and_cpu_isolation() {
    use ktime_types::{MonotonicInstant, TimeSpan};
    static SETS: [TimerSet<2>; 2] = [const { TimerSet::new() }; 2];
    for (timers, step) in SETS.iter().zip([10, 150_000_000]) {
        let now = Arc::new(AtomicU64::new(10));
        let calls = Arc::new(AtomicUsize::new(0));
        let clock = now.clone();
        let hits = calls.clone();
        let timer = Timer::new(timers, move |control: TimerControl<'_>, _: Expiration| {
            hits.fetch_add(1, Ordering::Relaxed);
            let next = clock.fetch_add(step, Ordering::Relaxed) + step;
            control
                .rearm(MonotonicInstant::from_span_since_origin(
                    TimeSpan::from_nanos(next),
                ))
                .unwrap();
        });
        let other = Timer::new(timers, |_: TimerControl<'_>, _: Expiration| {});
        let instant = |ns| MonotonicInstant::from_span_since_origin(TimeSpan::from_nanos(ns));
        // SAFETY: retained owners, serialized modeled service on each CPU.
        unsafe {
            timer.start_on(0.into(), instant(10)).unwrap();
            other.start_on(1.into(), instant(10)).unwrap();
        }
        for expected in [3, 6] {
            // SAFETY: sole dispatcher; callback advances a bounded monotonic clock.
            assert!(
                unsafe { timers.dispatch_due(0.into(), || now.load(Ordering::Relaxed)) }.unwrap()
            );
            assert_eq!(calls.load(Ordering::Relaxed), expected);
            let mut service = None;
            // SAFETY: synchronous observer with no nested locks or hardware access.
            unsafe { timers.reprogram_local_timer(0.into(), |next| service = next) }.unwrap();
            let service = service.unwrap();
            assert_eq!(
                service,
                now.load(Ordering::Relaxed) + (3 * step).min(100_000_000)
            );
            now.store(service - 1, Ordering::Relaxed);
            // SAFETY: monotonic serialized service during software timer deferral.
            assert!(
                !unsafe { timers.dispatch_due(0.into(), || now.load(Ordering::Relaxed)) }.unwrap()
            );
            assert_eq!(calls.load(Ordering::Relaxed), expected);
            now.store(service, Ordering::Relaxed);
        }
        // SAFETY: CPU1's independent timer must not inherit CPU0's deferral.
        unsafe {
            timers
                .reprogram_local_timer(1.into(), |next| assert_eq!(next, Some(10)))
                .unwrap();
            assert!(
                timers
                    .dispatch_due(1.into(), || now.load(Ordering::Relaxed))
                    .unwrap()
            );
        }
        assert!(timer.cancel_sync());
        // SAFETY: drained set, synchronous observer only.
        unsafe { timers.reprogram_local_timer(0.into(), |next| assert_eq!(next, None)) }.unwrap();
    }
}

#[test]
fn batch_handoff_observes_cleanup_time_and_keeps_callbacks_unlocked() {
    use ktime_types::{MonotonicInstant, TimeSpan};
    static TIMERS: TimerSet<1> = TimerSet::new();
    struct Cleanup(Arc<AtomicU64>);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            self.0.store(35, Ordering::Relaxed);
        }
    }
    let now = Arc::new(AtomicU64::new(10));
    let cleanup_clock = now.clone();
    let first = Timer::new(&TIMERS, move |_: TimerControl<'_>, _: Expiration| {
        let _cleanup = Cleanup(cleanup_clock.clone());
    });
    let counts = Arc::new(AtomicU64::new(0));
    let reported = counts.clone();
    let second = Timer::new(
        &TIMERS,
        move |timer: TimerControl<'_>, event: Expiration| {
            reported.store(event.deadline_ns, Ordering::Relaxed);
            // Both operations take the owner lock; dispatch must have released it.
            // SAFETY: synchronous modeled service, no hardware or retained snapshot.
            unsafe { TIMERS.reprogram_local_timer(0.into(), |_| {}) }.unwrap();
            timer
                .rearm(MonotonicInstant::from_span_since_origin(
                    TimeSpan::from_nanos(100),
                ))
                .unwrap();
        },
    );
    // SAFETY: single modeled CPU, live owned storage, no concurrent dispatcher.
    unsafe {
        first
            .start_on(
                0.into(),
                MonotonicInstant::from_span_since_origin(TimeSpan::from_nanos(5)),
            )
            .unwrap();
        second
            .start_on(
                0.into(),
                MonotonicInstant::from_span_since_origin(TimeSpan::from_nanos(10)),
            )
            .unwrap();
        TIMERS
            .dispatch_due(0.into(), || {
                // The clock must run outside the timer lock too.
                TIMERS.reprogram_local_timer(0.into(), |_| {}).unwrap();
                now.load(Ordering::Relaxed)
            })
            .unwrap();
    }
    assert_eq!(counts.load(Ordering::Relaxed), 10);
    assert_eq!(now.load(Ordering::Relaxed), 35);
    assert_eq!(first.try_cancel(), CancelResult::Inactive);
    assert!(second.cancel_sync());
}

#[test]
fn next_callback_can_destroy_previous_timer_after_batch_handoff() {
    use std::sync::Mutex;

    use ktime_types::{MonotonicInstant, TimeSpan};
    static TIMERS: TimerSet<1> = TimerSet::new();
    let first = Timer::new(&TIMERS, |_: TimerControl<'_>, _: Expiration| {});
    // SAFETY: owned storage, one modeled CPU, dispatch starts below.
    unsafe {
        first.start_on(
            0.into(),
            MonotonicInstant::from_span_since_origin(TimeSpan::from_nanos(5)),
        )
    }
    .unwrap();
    let owner = Arc::new(Mutex::new(Some(first)));
    let previous = owner.clone();
    let second = Timer::new(&TIMERS, move |_: TimerControl<'_>, _: Expiration| {
        drop(previous.lock().unwrap().take());
    });
    // SAFETY: exclusive modeled dispatch; the second callback releases only the
    // completed first timer, never its own storage or any active delivery.
    unsafe {
        second
            .start_on(
                0.into(),
                MonotonicInstant::from_span_since_origin(TimeSpan::from_nanos(10)),
            )
            .unwrap();
        TIMERS.dispatch_due(0.into(), || 10).unwrap();
    }
    assert!(owner.lock().unwrap().is_none());
    assert_eq!(second.try_cancel(), CancelResult::Inactive);
}

#[test]
fn horizon_is_pending_and_late_one_shot_delivers_once() {
    use ktime_types::{MonotonicInstant, TimeSpan};
    static TIMERS: TimerSet<1> = TimerSet::new();
    let count = Arc::new(AtomicUsize::new(0));
    let observed = count.clone();
    let timer = Timer::new(&TIMERS, move |_: TimerControl<'_>, event: Expiration| {
        assert_eq!(event.deadline_ns, 0);
        observed.fetch_add(1, Ordering::Relaxed);
    });
    let instant = |ns| MonotonicInstant::from_span_since_origin(TimeSpan::from_nanos(ns));
    // SAFETY: sole modeled service; storage retained, clock below the horizon.
    unsafe {
        timer.start_on(0.into(), instant(u64::MAX)).unwrap();
        assert!(!TIMERS.dispatch_due(0.into(), || u64::MAX - 1).unwrap());
        TIMERS
            .reprogram_local_timer(0.into(), |next| assert_eq!(next, Some(u64::MAX)))
            .unwrap();
        timer.start_on(0.into(), instant(0)).unwrap();
        assert!(TIMERS.dispatch_due(0.into(), || u64::MAX - 1).unwrap());
        assert!(!TIMERS.dispatch_due(0.into(), || u64::MAX - 1).unwrap());
    }
    assert_eq!(count.load(Ordering::Relaxed), 1);
    assert!(!timer.cancel_sync());
}

#[test]
fn embedded_handler_is_shared_across_shell_move_and_dispatch() {
    use ktime_types::{MonotonicInstant, TimeSpan};
    static TIMERS: TimerSet<1> = TimerSet::new();
    struct Handler(SpinNoIrq<usize>);
    impl TimerHandler for Handler {
        fn on_expiration(&self, _: TimerControl<'_>, _: Expiration) {
            *self.0.lock() += 1;
        }
    }
    let timer = Timer::new(&TIMERS, Handler(SpinNoIrq::new(0)));
    let address = core::ptr::from_ref(timer.handler());
    let moved = timer;
    assert_eq!(address, core::ptr::from_ref(moved.handler()));
    // SAFETY: live owning timer, one modeled CPU serviced by the scoped thread.
    unsafe {
        moved.start_on(
            0.into(),
            MonotonicInstant::from_span_since_origin(TimeSpan::from_nanos(10)),
        )
    }
    .unwrap();
    let handler = moved.handler();
    std::thread::scope(|scope| {
        let dispatch = scope.spawn(|| {
            // SAFETY: sole modeled dispatcher; the owner outlives this scope.
            unsafe { TIMERS.dispatch_due(0.into(), || 10) }.unwrap();
        });
        assert!(*handler.0.lock() <= 1);
        dispatch.join().unwrap();
    });
    assert_eq!(*handler.0.lock(), 1);
    assert_eq!(moved.try_cancel(), CancelResult::Inactive);
}

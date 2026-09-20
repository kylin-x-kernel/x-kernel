// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Allocation acceptance through the owned API, separate from timing samples.

use std::sync::Arc;

use super::{
    ALLOCS, COUNTING, Expiration, FREES, MonotonicInstant, Ordering, TimeSpan, TimerControl,
    TimerSet,
};
use crate::timer::{CancelResult, Timer};

fn counted(f: impl FnOnce()) -> (usize, usize) {
    ALLOCS.store(0, Ordering::Relaxed);
    FREES.store(0, Ordering::Relaxed);
    COUNTING.store(true, Ordering::Relaxed);
    f();
    COUNTING.store(false, Ordering::Relaxed);
    (
        ALLOCS.load(Ordering::Relaxed),
        FREES.load(Ordering::Relaxed),
    )
}

fn deadline(ns: u64) -> MonotonicInstant {
    MonotonicInstant::from_span_since_origin(TimeSpan::from_nanos(ns))
}

pub(super) fn run() {
    // A nonzero control detects a disabled or optimized-away allocation probe.
    let control = counted(|| drop(std::hint::black_box(Box::new(42_u64))));
    assert_eq!(control, (1, 1));

    static TIMERS: TimerSet<2> = TimerSet::new();
    let calls = Arc::new(super::AtomicUsize::new(0));
    let hits = calls.clone();
    let child = Arc::new(Timer::new(&TIMERS, |_: TimerControl<'_>, _: Expiration| {
        panic!("canceled child must not run")
    }));
    let callback_child = child.clone();
    let timer = Timer::new(
        &TIMERS,
        move |control: TimerControl<'_>, event: Expiration| {
            hits.fetch_add(1, Ordering::Relaxed);
            // SAFETY: dispatch on modeled CPU0 is serialized. Prepared child
            // storage remains owned; start does not recursively dispatch it.
            unsafe { callback_child.start_on(0.into(), deadline(0)) }.unwrap();
            // SAFETY: same collection, live storage and serialized dispatch.
            unsafe { callback_child.start_on(0.into(), deadline(0)) }.unwrap();
            assert_eq!(callback_child.try_cancel(), CancelResult::Removed);
            assert_eq!(callback_child.try_cancel(), CancelResult::Inactive);
            // SAFETY: same modeled CPU; no other dispatcher can run the child.
            unsafe { callback_child.start_on(0.into(), deadline(0)) }.unwrap();
            assert!(callback_child.cancel_sync());
            if event.deadline_ns == 10 {
                assert!(
                    control
                        .rearm(MonotonicInstant::from_span_since_origin(
                            TimeSpan::try_from_nanos(u128::from(u64::MAX) + 1).unwrap()
                        ))
                        .is_err()
                );
                control.rearm(deadline(100)).unwrap();
            } else {
                assert_eq!(event.deadline_ns, 100);
            }
        },
    );
    let beyond = TimeSpan::try_from_nanos(u128::from(u64::MAX) + 1).unwrap();
    let operations = counted(|| {
        assert_eq!(timer.try_cancel(), CancelResult::Inactive);
        assert!(!timer.cancel_sync());
        // SAFETY: retained owned storage, static collection, serialized modeled
        // CPUs and callbacks; this harness does not use real IRQ or hardware.
        unsafe { timer.start_on(0.into(), deadline(10)) }.unwrap();
        // SAFETY: same modeled platform; invalid time must not mutate state.
        assert!(
            unsafe { timer.start_on(0.into(), MonotonicInstant::from_span_since_origin(beyond)) }
                .is_err()
        );
        // SAFETY: invalid CPU is rejected before mutation; storage remains live.
        assert!(unsafe { timer.start_on(2.into(), deadline(20)) }.is_err());
        // SAFETY: monotonic modeled clock with no nested operations or locks.
        assert_eq!(
            unsafe { timer.start_on_if_future(1.into(), deadline(5), || 5) }.unwrap(),
            None
        );
        // SAFETY: exclusive modeled dispatch; the owning timer remains live.
        assert!(!unsafe { TIMERS.dispatch_due(0.into(), || 9) }.unwrap());
        // SAFETY: same modeled CPU; callback rearm uses its scoped control.
        assert!(unsafe { TIMERS.dispatch_due(0.into(), || 10) }.unwrap());
        // SAFETY: monotonic modeled time; the explicit successor must survive
        // rejected replacements and deliver once even when service is late.
        assert!(unsafe { TIMERS.dispatch_due(0.into(), || 135) }.unwrap());
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        assert_eq!(timer.try_cancel(), CancelResult::Inactive);
        // SAFETY: idle node may select CPU1; the locked clock precedes its deadline.
        assert_eq!(
            unsafe { timer.start_on_if_future(1.into(), deadline(200), || 135) }.unwrap(),
            Some((1.into(), true))
        );
        // SAFETY: no delivery is running; retained pending storage can change CPU.
        assert_eq!(
            unsafe { timer.start_on(0.into(), deadline(300)) }.unwrap(),
            (0.into(), false)
        );
        assert!(timer.cancel_sync());
        for i in 0..4096 {
            // SAFETY: sequential reuse on modeled CPU0; no real IRQ can dispatch.
            unsafe { timer.start_on(0.into(), deadline(400 + i)) }.unwrap();
            if i % 2 == 0 {
                assert_eq!(timer.try_cancel(), CancelResult::Removed);
            } else {
                assert!(timer.cancel_sync());
            }
        }
        // SAFETY: both modeled CPU queues are drained; observer only asserts.
        unsafe {
            TIMERS
                .reprogram_local_timer(0.into(), |next| assert_eq!(next, None))
                .unwrap();
            TIMERS
                .reprogram_local_timer(1.into(), |next| assert_eq!(next, None))
                .unwrap();
        }
    });
    assert_eq!(
        operations,
        (0, 0),
        "owned operations must not allocate or free"
    );
    // Final storage destruction is intentionally outside the operation phase.
    // The external calls Arc stays live, so exactly TimerData is freed here.
    let teardown = counted(|| drop(timer));
    assert_eq!(teardown, (0, 1));
    assert_eq!(Arc::strong_count(&calls), 1);
    assert_eq!(Arc::strong_count(&child), 1);
    println!("owned API: operation allocs=0 frees=0; final storage frees=1; probe control=1/1");
}

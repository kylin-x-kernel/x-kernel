// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Concurrent public-API checks with modeled CPU service, also run under Miri.

use std::sync::Barrier;

use ktime_types::{MonotonicInstant, TimeSpan};

use super::*;

fn instant(ns: u64) -> MonotonicInstant {
    MonotonicInstant::from_span_since_origin(TimeSpan::from_nanos(ns))
}

fn deadlines<const N: usize>(timers: &TimerSet<N>) -> [Option<u64>; N] {
    core::array::from_fn(|cpu| {
        let mut next = None;
        // SAFETY: synchronous modeled observation, with no hardware or nested
        // timer operations. The snapshot is used only after writers have joined.
        unsafe { timers.reprogram_local_timer(cpu.into(), |value| next = value) }.unwrap();
        next
    })
}

#[test]
fn pending_migration_updates_both_minima_and_delivers_only_at_destination() {
    static TIMERS: TimerSet<2> = TimerSet::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let timer = Timer::new(&TIMERS, move |_: TimerControl<'_>, event: Expiration| {
        assert_eq!(event.deadline_ns, 40);
        observed.fetch_add(1, Ordering::Relaxed);
    });
    let other = Timer::new(&TIMERS, |_: TimerControl<'_>, _: Expiration| {});
    for source in [0, 1] {
        let destination = 1 - source;
        // SAFETY: retained storage, sequential modeled CPUs and no hardware.
        unsafe {
            assert_eq!(
                timer.start_on(source.into(), instant(10)),
                Ok((source.into(), true))
            );
            other.start_on(source.into(), instant(30)).unwrap();
            assert_eq!(
                timer.start_on(destination.into(), instant(40)),
                Ok((destination.into(), false))
            );
        }
        let pending = deadlines(&TIMERS);
        assert_eq!(pending[source], Some(30));
        assert_eq!(pending[destination], Some(40));
        assert_eq!(other.try_cancel(), CancelResult::Removed);
        // SAFETY: one sequential dispatcher per modeled CPU, owners remain live.
        unsafe {
            assert!(!TIMERS.dispatch_due(source.into(), || 100).unwrap());
            assert!(!TIMERS.dispatch_due(destination.into(), || 39).unwrap());
            assert!(TIMERS.dispatch_due(destination.into(), || 40).unwrap());
            assert!(!TIMERS.dispatch_due(destination.into(), || 100).unwrap());
        }
        assert_eq!(deadlines(&TIMERS), [None, None]);
    }
    assert_eq!(calls.load(Ordering::Relaxed), 2);
}

#[test]
fn future_only_migration_rejects_without_change_then_replaces() {
    static TIMERS: TimerSet<2> = TimerSet::new();
    let timer = Timer::new(&TIMERS, |_: TimerControl<'_>, _: Expiration| {});
    for source in [0, 1] {
        let destination = 1 - source;
        // SAFETY: retained storage, sequential modeled service and constant clocks.
        unsafe {
            timer.start_on(source.into(), instant(100)).unwrap();
            assert_eq!(
                timer.start_on_if_future(destination.into(), instant(10), || 10),
                Ok(None)
            );
            assert!(timer.start_on(2.into(), instant(20)).is_err());
            assert_eq!(deadlines(&TIMERS)[source], Some(100));
            assert_eq!(deadlines(&TIMERS)[destination], None);
            let mut clock_calls = 0;
            assert_eq!(
                timer.start_on_if_future(destination.into(), instant(20), || {
                    clock_calls += 1;
                    10
                }),
                Ok(Some((destination.into(), false)))
            );
            assert_eq!(clock_calls, 1);
        }
        assert_eq!(deadlines(&TIMERS)[source], None);
        assert_eq!(deadlines(&TIMERS)[destination], Some(20));
        assert!(timer.cancel_sync());
        assert_eq!(deadlines(&TIMERS), [None, None]);
    }
}

#[test]
fn explicit_one_shot_repetition_stops_without_rearming() {
    static TIMERS: TimerSet<1> = TimerSet::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let timer = Timer::new(
        &TIMERS,
        move |control: TimerControl<'_>, event: Expiration| {
            let completed = observed.fetch_add(1, Ordering::Relaxed) + 1;
            if completed < 3 {
                control.rearm(instant(event.deadline_ns + 10)).unwrap();
            }
        },
    );
    // SAFETY: static set, retained owning timer, serialized modeled CPU service.
    unsafe { timer.start_on(0.into(), instant(10)) }.unwrap();
    for now in [10, 20, 30] {
        assert_eq!(deadlines(&TIMERS), [Some(now)]);
        // SAFETY: sole modeled dispatcher; handler only updates its atomic and
        // rearms this live timer, without hardware access or blocking.
        assert!(unsafe { TIMERS.dispatch_due(0.into(), || now) }.unwrap());
    }
    assert_eq!(calls.load(Ordering::Relaxed), 3);
    assert_eq!(deadlines(&TIMERS), [None]);
    // SAFETY: unchanged lifetime and serialized service; time advances monotonically.
    assert!(!unsafe { TIMERS.dispatch_due(0.into(), || 100) }.unwrap());
    assert_eq!(timer.try_cancel(), CancelResult::Inactive);
    drop(timer);
    assert_eq!(Arc::strong_count(&calls), 1);
}

#[test]
fn explicit_rearm_and_remote_start_follow_lock_order_not_callback_return() {
    static TIMERS: TimerSet<2> = TimerSet::new();
    for callback_first in [true, false] {
        let entered = Arc::new(Barrier::new(2));
        let replaced = Arc::new(Barrier::new(2));
        let entered_callback = entered.clone();
        let replaced_callback = replaced.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let rearm_succeeded = Arc::new(AtomicBool::new(false));
        let callback_result = rearm_succeeded.clone();
        let timer = Timer::new(&TIMERS, move |control: TimerControl<'_>, _: Expiration| {
            if observed.fetch_add(1, Ordering::Relaxed) != 0 {
                return;
            }
            if callback_first {
                callback_result.store(control.rearm(instant(100)).is_ok(), Ordering::Relaxed);
            }
            entered_callback.wait();
            replaced_callback.wait();
            if !callback_first {
                callback_result.store(control.rearm(instant(100)).is_ok(), Ordering::Relaxed);
            }
        });
        // SAFETY: retained timer and static set; one modeled CPU0 dispatcher.
        unsafe { timer.start_on(0.into(), instant(10)) }.unwrap();
        let dispatch = std::thread::spawn(|| {
            // SAFETY: sole modeled CPU0 service with live owning storage. The
            // barriers model an in-flight callback, not production IRQ behavior.
            unsafe { TIMERS.dispatch_due(0.into(), || 10) }.unwrap()
        });
        entered.wait();
        // SAFETY: remote replacement of retained storage; CPU1 never dispatches.
        let result = unsafe { timer.start_on(1.into(), instant(200)) };
        let cancellation = timer.try_cancel();
        // Release and join before checking results so assertions cannot strand
        // the callback and make the owning timer's destructor wait forever.
        replaced.wait();
        assert!(dispatch.join().unwrap());
        assert_eq!(result, Ok((0.into(), !callback_first)));
        assert_eq!(cancellation, CancelResult::Running);
        assert!(rearm_succeeded.load(Ordering::Relaxed));
        let expected = if callback_first { 200 } else { 100 };
        assert_eq!(deadlines(&TIMERS), [Some(expected), None]);
        // SAFETY: worker joined; sequential modeled service, with storage retained.
        assert!(unsafe { TIMERS.dispatch_due(0.into(), || expected) }.unwrap());
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        assert_eq!(deadlines(&TIMERS), [None, None]);
        assert_eq!(timer.try_cancel(), CancelResult::Inactive);
        drop(timer);
        assert_eq!(Arc::strong_count(&calls), 1);
    }
}

#[test]
fn concurrent_replacements_deliver_one_complete_setting_and_release_storage() {
    static TIMERS: TimerSet<2> = TimerSet::new();
    struct Handler {
        event: SpinNoIrq<Option<Expiration>>,
        drops: Arc<AtomicUsize>,
    }
    impl TimerHandler for Handler {
        fn on_expiration(&self, _: TimerControl<'_>, event: Expiration) {
            assert!(self.event.lock().replace(event).is_none());
        }
    }
    impl Drop for Handler {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::Relaxed);
        }
    }
    let drops = Arc::new(AtomicUsize::new(0));
    for round in 0..16 {
        let timer = Timer::new(
            &TIMERS,
            Handler {
                event: SpinNoIrq::new(None),
                drops: drops.clone(),
            },
        );
        let barrier = Barrier::new(2);
        let first = [round as u64 * 1000 + 100, round as u64 * 1000 + 200];
        let owners = std::thread::scope(|scope| {
            let workers = [0, 1].map(|cpu| {
                let timer = &timer;
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    // SAFETY: retained owning storage, two concurrent modeled
                    // callers, no dispatcher until both have joined. Routing
                    // serializes each complete deadline and CPU selection.
                    unsafe { timer.start_on(cpu.into(), instant(first[cpu])) }
                        .unwrap()
                        .0
                })
            });
            workers.map(|worker| worker.join().unwrap())
        });
        assert_eq!(owners, [0.into(), 1.into()]);
        assert!(timer.handler().event.lock().is_none());
        let pending = deadlines(&TIMERS);
        let owner = pending.iter().position(Option::is_some).unwrap();
        assert_eq!(pending[1 - owner], None);
        let selected = first
            .iter()
            .position(|&ns| Some(ns) == pending[owner])
            .unwrap();
        assert_eq!(selected, owner);
        let now = first[selected] + 33;
        // SAFETY: serialized modeled CPU service after both writers joined;
        // callback storage is retained and late service consumes one registration.
        unsafe {
            assert!(TIMERS.dispatch_due(owner.into(), || now).unwrap());
            assert!(!TIMERS.dispatch_due((1 - owner).into(), || now).unwrap());
            assert!(!TIMERS.dispatch_due(owner.into(), || now).unwrap());
        }
        assert_eq!(
            *timer.handler().event.lock(),
            Some(Expiration {
                deadline_ns: first[selected],
            })
        );
        assert_eq!(deadlines(&TIMERS)[owner], None);
        assert!(!timer.cancel_sync());
        assert_eq!(deadlines(&TIMERS), [None, None]);
        drop(timer);
        assert_eq!(drops.load(Ordering::Relaxed), round + 1);
        assert_eq!(Arc::strong_count(&drops), 1);
    }
}

#[test]
fn running_replacements_preserve_latest_deadline_and_owner() {
    static TIMERS: TimerSet<2> = TimerSet::new();
    struct State {
        entered: AtomicBool,
        can_finish: AtomicBool,
        events: SpinNoIrq<[Option<Expiration>; 3]>,
    }
    let state = Arc::new(State {
        entered: AtomicBool::new(false),
        can_finish: AtomicBool::new(false),
        events: SpinNoIrq::new([None; 3]),
    });
    let shared = state.clone();
    let timer = Timer::new(
        &TIMERS,
        move |control: TimerControl<'_>, event: Expiration| {
            if event.deadline_ns == 10 {
                shared.entered.store(true, Ordering::Release);
                while !shared.can_finish.load(Ordering::Acquire) {
                    std::thread::yield_now();
                }
                // A failed callback-side update must not revoke the remote successor.
                assert!(
                    control
                        .rearm(MonotonicInstant::from_span_since_origin(
                            TimeSpan::try_from_nanos(u128::from(u64::MAX) + 1).unwrap()
                        ))
                        .is_err()
                );
            }
            shared.events.lock()[match event.deadline_ns {
                10 => 0,
                200 => 1,
                400 => 2,
                _ => panic!("unexpected delivery"),
            }] = Some(event);
        },
    );
    // SAFETY: retained owner and a single dispatcher for modeled CPU0 below.
    unsafe { timer.start_on(0.into(), instant(10)) }.unwrap();
    let dispatch = std::thread::spawn(|| {
        // SAFETY: exclusive modeled CPU0 service; the owner stays alive until join.
        unsafe { TIMERS.dispatch_due(0.into(), || 10) }.unwrap();
    });
    while !state.entered.load(Ordering::Acquire) {
        std::thread::yield_now();
    }
    // Save results and release the callback before asserting, so a failed
    // expectation cannot strand the dispatcher inside this test's handshake.
    // SAFETY: concurrent replacement on a live owned timer; no CPU1 dispatcher.
    let results = unsafe {
        [
            timer.start_on(1.into(), instant(100)),
            timer.start_on(1.into(), instant(200)),
        ]
    };
    // SAFETY: invalid input is rejected; lifetime/context are unchanged.
    let rejected = unsafe {
        timer.start_on(
            1.into(),
            MonotonicInstant::from_span_since_origin(
                TimeSpan::try_from_nanos(u128::from(u64::MAX) + 1).unwrap(),
            ),
        )
    };
    let cancellation = timer.try_cancel();
    state.can_finish.store(true, Ordering::Release);
    dispatch.join().unwrap();
    assert_eq!(results, [Ok((0.into(), true)), Ok((0.into(), false))]);
    assert!(rejected.is_err());
    assert_eq!(cancellation, CancelResult::Running);
    assert_eq!(deadlines(&TIMERS), [Some(200), None]);
    assert_eq!(state.events.lock()[0], Some(Expiration { deadline_ns: 10 }));
    // SAFETY: all later service is serialized, with retained owned storage.
    unsafe {
        assert!(!TIMERS.dispatch_due(1.into(), || 200).unwrap());
        assert!(TIMERS.dispatch_due(0.into(), || 200).unwrap());
    }
    assert_eq!(
        state.events.lock()[1],
        Some(Expiration { deadline_ns: 200 })
    );
    assert_eq!(deadlines(&TIMERS), [None, None]);
    assert!(!timer.cancel_sync());
    // SAFETY: idle restart may select CPU1; neither CPU is concurrently serviced.
    unsafe {
        assert_eq!(timer.start_on(1.into(), instant(400)), Ok((1.into(), true)));
        assert!(!TIMERS.dispatch_due(0.into(), || 400).unwrap());
        assert!(TIMERS.dispatch_due(1.into(), || 400).unwrap());
    }
    assert_eq!(
        state.events.lock()[2],
        Some(Expiration { deadline_ns: 400 })
    );
    assert_eq!(timer.try_cancel(), CancelResult::Inactive);
    assert_eq!(deadlines(&TIMERS), [None, None]);
    drop(timer);
    assert_eq!(Arc::strong_count(&state), 1);
}

#[test]
#[ignore = "explicit native contention stress; not a deterministic unit test"]
fn cross_cpu_start_cancel_dispatch_stress() {
    static TIMERS: TimerSet<2> = TimerSet::new();
    const REQUESTS_PER_WRITER: usize = 100_000;
    struct Handler {
        active: AtomicUsize,
        deliveries: Box<[AtomicUsize]>,
        drops: Arc<AtomicUsize>,
    }
    impl TimerHandler for Handler {
        fn on_expiration(&self, _: TimerControl<'_>, event: Expiration) {
            assert_eq!(self.active.fetch_add(1, Ordering::Relaxed), 0);
            assert_eq!(
                self.deliveries[event.deadline_ns as usize].fetch_add(1, Ordering::Relaxed),
                0
            );
            assert_eq!(self.active.fetch_sub(1, Ordering::Relaxed), 1);
        }
    }
    impl Drop for Handler {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::Relaxed);
        }
    }
    let drops = Arc::new(AtomicUsize::new(0));
    let timer = Timer::new(
        &TIMERS,
        Handler {
            active: AtomicUsize::new(0),
            deliveries: (0..2 * REQUESTS_PER_WRITER + 2)
                .map(|_| AtomicUsize::new(0))
                .collect(),
            drops: drops.clone(),
        },
    );
    let barrier = Barrier::new(4);
    let remaining = AtomicUsize::new(2);
    let now = AtomicU64::new(1);
    std::thread::scope(|scope| {
        for cpu in 0..2 {
            let timer = &timer;
            let barrier = &barrier;
            let remaining = &remaining;
            let now = &now;
            scope.spawn(move || {
                barrier.wait();
                for request in 0..REQUESTS_PER_WRITER {
                    let request_id = cpu * REQUESTS_PER_WRITER + request;
                    let deadline = request_id as u64;
                    // SAFETY: stable owning timer, one dispatcher per modeled
                    // CPU, bounded shared clock. Owner routing serializes the
                    // competing writers with migration and active delivery.
                    unsafe { timer.start_on(cpu.into(), instant(deadline)) }.unwrap();
                    match request % 3 {
                        0 => {
                            let _ = timer.try_cancel();
                        }
                        1 => {
                            timer.cancel_sync();
                        }
                        _ => {}
                    }
                }
                remaining.fetch_sub(1, Ordering::Release);
            });
            scope.spawn(move || {
                barrier.wait();
                while remaining.load(Ordering::Acquire) != 0 {
                    // SAFETY: this is the only dispatcher for this modeled CPU.
                    // The shared clock is monotonic and owners outlive the scope.
                    unsafe {
                        TIMERS.dispatch_due(cpu.into(), || now.fetch_add(1, Ordering::Relaxed))
                    }
                    .unwrap();
                    std::thread::yield_now();
                }
            });
        }
    });
    timer.cancel_sync();
    assert_eq!(timer.handler().active.load(Ordering::Relaxed), 0);
    assert_eq!(deadlines(&TIMERS), [None, None]);
    // With contention stopped, require one exact delivery on each reselected CPU.
    for cpu in 0..2 {
        let request_id = 2 * REQUESTS_PER_WRITER + cpu;
        let deadline = request_id as u64;
        // SAFETY: all workers joined; serialized service and a live owner.
        unsafe {
            assert_eq!(
                timer.start_on(cpu.into(), instant(deadline)),
                Ok((cpu.into(), true))
            );
            assert!(
                TIMERS
                    .dispatch_due(cpu.into(), || now.load(Ordering::Relaxed).max(deadline))
                    .unwrap()
            );
        }
        assert_eq!(
            timer.handler().deliveries[request_id].load(Ordering::Relaxed),
            1
        );
    }
    assert_eq!(deadlines(&TIMERS), [None, None]);
    drop(timer);
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    assert_eq!(Arc::strong_count(&drops), 1);
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Host algorithm comparison, not an execution of the old kernel image.
//! Legacy insert/remove/split-off follows task/ktask/src/future/time.rs at
//! main 45386bd06f3f962671b8785d77c9aeba93523c88. CPU routing and hardware programming
//! are excluded from both sides; the legacy clock is a supplied test instant.

use std::{
    collections::BTreeMap,
    sync::Arc,
    task::{Wake, Waker},
};

use super::{
    ALLOCS, AtomicUsize, COUNTING, Expiration, FREES, Instant, MonotonicInstant, Ordering,
    SpinNoIrq, TimeSpan, TimerControl, TimerHandler, TimerSet,
};
use crate::timer::Timer;

#[path = "sleep_state.rs"]
mod sleep_state;

pub(super) fn compare_sleep_state(events: usize) {
    sleep_state::run(events);
}

struct Hits(AtomicUsize);
impl Wake for Hits {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

fn instant(ns: u64) -> MonotonicInstant {
    MonotonicInstant::from_span_since_origin(TimeSpan::from_nanos(ns))
}

trait Queue {
    fn prepare(n: usize, waker: &Waker) -> Self;
    fn arm(&mut self, slot: usize, deadline_ns: u64, now_ns: u64);
    fn cancel(&mut self, slot: usize);
    fn dispatch(&mut self, now_ns: u64);
}

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
struct LegacyKey {
    deadline: MonotonicInstant,
    key: u64,
}

struct LegacyRuntime {
    key: u64,
    wheel: BTreeMap<LegacyKey, Waker>,
}

struct Legacy {
    runtime: SpinNoIrq<LegacyRuntime>,
    handles: Box<[Option<LegacyKey>]>,
    waker: Waker,
}

impl Queue for Legacy {
    fn prepare(n: usize, waker: &Waker) -> Self {
        Self {
            runtime: SpinNoIrq::new(LegacyRuntime {
                key: 0,
                wheel: BTreeMap::new(),
            }),
            handles: vec![None; n].into_boxed_slice(),
            waker: waker.clone(),
        }
    }

    fn arm(&mut self, slot: usize, deadline_ns: u64, now_ns: u64) {
        let mut runtime = self.runtime.lock();
        let deadline = instant(deadline_ns);
        assert!(deadline > instant(now_ns));
        let key = LegacyKey {
            deadline,
            key: runtime.key,
        };
        runtime.wheel.insert(key, self.waker.clone());
        runtime.key += 1;
        self.handles[slot] = Some(key);
    }

    fn cancel(&mut self, slot: usize) {
        let key = self.handles[slot].take().unwrap();
        assert!(self.runtime.lock().wheel.remove(&key).is_some());
    }

    fn dispatch(&mut self, now_ns: u64) {
        let expired = {
            let mut runtime = self.runtime.lock();
            let pending = runtime.wheel.split_off(&LegacyKey {
                deadline: instant(now_ns),
                key: u64::MAX,
            });
            std::mem::replace(&mut runtime.wheel, pending)
        };
        for (_, waker) in expired {
            waker.wake();
        }
    }
}

struct WakeHandler(Waker);
impl TimerHandler for WakeHandler {
    fn on_expiration(&self, _: TimerControl<'_>, _: Expiration) {
        self.0.wake_by_ref();
    }
}

static TIMERS: TimerSet<1> = TimerSet::new();
struct Prepared {
    timers: Box<[Timer<WakeHandler, 1>]>,
}

impl Queue for Prepared {
    fn prepare(n: usize, waker: &Waker) -> Self {
        Self {
            timers: (0..n)
                .map(|_| Timer::new(&TIMERS, WakeHandler(waker.clone())))
                .collect(),
        }
    }

    fn arm(&mut self, slot: usize, deadline_ns: u64, now_ns: u64) {
        // SAFETY: retained owned storage, static set and one modeled dispatcher.
        // The supplied bounded clock takes no locks or allocations.
        assert!(
            unsafe {
                self.timers[slot].start_on_if_future(0.into(), instant(deadline_ns), || now_ns)
            }
            .unwrap()
            .is_some()
        );
    }

    fn cancel(&mut self, slot: usize) {
        assert!(self.timers[slot].cancel_sync());
    }

    fn dispatch(&mut self, now_ns: u64) {
        // SAFETY: serialized simulated CPU0 service, bounded monotonic clock,
        // retained owners and an atomic-only callback through the common waker.
        assert!(unsafe { TIMERS.dispatch_due(0.into(), || now_ns) }.unwrap());
    }
}

fn sample<Q: Queue>(case: &str, n: usize, events: usize, count: bool) -> (u128, usize, usize) {
    let hits = Arc::new(Hits(AtomicUsize::new(0)));
    let waker = Waker::from(hits.clone());
    let mut queue = (case != "lifecycle").then(|| Q::prepare(n, &waker));
    ALLOCS.store(0, Ordering::Relaxed);
    FREES.store(0, Ordering::Relaxed);
    let mut elapsed = 0;
    for batch in 0..events / n {
        let now = 2 * batch as u64;
        let deadline = now + 1;
        if case == "dispatch" {
            for slot in 0..n {
                queue.as_mut().unwrap().arm(slot, deadline, now);
            }
        }
        COUNTING.store(count, Ordering::Relaxed);
        let started = Instant::now();
        if case == "lifecycle" {
            let mut q = Q::prepare(n, &waker);
            for slot in 0..n {
                q.arm(slot, deadline, now);
            }
            q.dispatch(deadline);
            drop(q);
        } else {
            let q = queue.as_mut().unwrap();
            if case == "start_cancel" {
                for slot in 0..n {
                    q.arm(slot, deadline, now);
                }
                for slot in 0..n {
                    q.cancel(slot);
                }
            } else {
                q.dispatch(deadline);
            }
        }
        elapsed += started.elapsed().as_nanos();
        COUNTING.store(false, Ordering::Relaxed);
    }
    let allocations = ALLOCS.load(Ordering::Relaxed);
    let frees = FREES.load(Ordering::Relaxed);
    drop(queue);
    assert_eq!(
        hits.0.load(Ordering::Relaxed),
        if case == "start_cancel" { 0 } else { events }
    );
    assert_eq!(Arc::strong_count(&hits), 2);
    (elapsed, allocations, frees)
}

pub(super) fn run(events: usize) {
    assert!((4096..=1_048_576).contains(&events) && events.is_multiple_of(4096));
    println!("mode,version,case,n,round,operations,elapsed_ns,allocs,frees");
    for n in [1, 32, 256, 4096] {
        for case in ["start_cancel", "dispatch", "lifecycle"] {
            sample::<Legacy>(case, n, 4096, false);
            sample::<Prepared>(case, n, 4096, false);
            // ABBA order reduces drift bias; allocation probes are separate so
            // atomic counter overhead cannot inflate old allocator timing.
            for round in 0..3 {
                for version in ["legacy", "prepared", "prepared", "legacy"] {
                    let (ns, ..) = if version == "legacy" {
                        sample::<Legacy>(case, n, events, false)
                    } else {
                        sample::<Prepared>(case, n, events, false)
                    };
                    println!("timing,{version},{case},{n},{round},{events},{ns},0,0");
                }
            }
            for version in ["legacy", "prepared"] {
                let (ns, allocs, frees) = if version == "legacy" {
                    sample::<Legacy>(case, n, events, true)
                } else {
                    sample::<Prepared>(case, n, events, true)
                };
                if version == "prepared" && case != "lifecycle" {
                    assert_eq!((allocs, frees), (0, 0));
                }
                println!("allocation,{version},{case},{n},0,{events},{ns},{allocs},{frees}");
            }
        }
    }
}

/// Diagnostic phase timings; additional timestamps perturb the lifecycle.
/// Use --compare without phase timestamps to accept or reject an optimization.
pub(super) fn profile(events: usize) {
    eprintln!(
        "layout: Deadline={} TimerNode={} Expiration={}",
        core::mem::size_of::<crate::deadline::Deadline>(),
        core::mem::size_of::<crate::index::TimerNode>(),
        core::mem::size_of::<Expiration>(),
    );
    assert!((4096..=1_048_576).contains(&events) && events.is_multiple_of(4096));
    fn phases<Q: Queue>(n: usize, events: usize) -> [u128; 4] {
        let hits = Arc::new(Hits(AtomicUsize::new(0)));
        let waker = Waker::from(hits.clone());
        let mut elapsed = [0; 4];
        COUNTING.store(false, Ordering::Relaxed);
        for batch in 0..events / n {
            let now = 2 * batch as u64;
            let start = Instant::now();
            let mut queue = Q::prepare(n, &waker);
            let prepared = Instant::now();
            for slot in 0..n {
                queue.arm(slot, now + 1, now);
            }
            let armed = Instant::now();
            queue.dispatch(now + 1);
            let delivered = Instant::now();
            drop(queue);
            let destroyed = Instant::now();
            for (total, time) in elapsed.iter_mut().zip([
                prepared.duration_since(start),
                armed.duration_since(prepared),
                delivered.duration_since(armed),
                destroyed.duration_since(delivered),
            ]) {
                *total += time.as_nanos();
            }
        }
        assert_eq!(hits.0.load(Ordering::Relaxed), events);
        assert_eq!(Arc::strong_count(&hits), 2);
        elapsed
    }
    println!("version,n,round,phase,operations,elapsed_ns");
    for n in [1, 32, 256, 4096] {
        phases::<Legacy>(n, 4096);
        phases::<Prepared>(n, 4096);
        for round in 0..3 {
            for version in ["legacy", "prepared", "prepared", "legacy"] {
                let times = if version == "legacy" {
                    phases::<Legacy>(n, events)
                } else {
                    phases::<Prepared>(n, events)
                };
                for (phase, ns) in ["prepare", "arm", "dispatch", "destroy"]
                    .into_iter()
                    .zip(times)
                {
                    println!("{version},{n},{round},{phase},{events},{ns}");
                }
            }
        }
    }
}

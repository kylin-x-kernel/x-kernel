// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Source-derived sleep-state layouts around the real owning Timer.
//! No real Future scheduling, IRQ delivery or platform reprogramming is timed.

use std::{sync::Arc, task::Waker};

use super::{
    Expiration, Queue, SpinNoIrq, TIMERS, Timer, TimerControl, TimerHandler, instant, sample,
};

struct State {
    is_expired: bool,
    waker: Option<Waker>,
}

fn state(waker: &Waker) -> SpinNoIrq<State> {
    SpinNoIrq::new(State {
        is_expired: false,
        waker: Some(waker.clone()),
    })
}

fn complete(state: &SpinNoIrq<State>) {
    let waker = {
        let mut state = state.lock();
        state.is_expired = true;
        state.waker.take()
    };
    if let Some(waker) = waker {
        waker.wake();
    }
}

struct SharedHandler(Arc<SpinNoIrq<State>>);
impl TimerHandler for SharedHandler {
    fn on_expiration(&self, _: TimerControl<'_>, _: Expiration) {
        complete(&self.0);
    }
}

struct EmbeddedHandler(SpinNoIrq<State>);
impl TimerHandler for EmbeddedHandler {
    fn on_expiration(&self, _: TimerControl<'_>, _: Expiration) {
        complete(&self.0);
    }
}

type SeparateWait = (Timer<SharedHandler, 1>, Arc<SpinNoIrq<State>>);
struct Separate {
    waits: Box<[SeparateWait]>,
}
struct Embedded {
    waits: Box<[Timer<EmbeddedHandler, 1>]>,
}

fn arm<H: TimerHandler + 'static>(timer: &Timer<H, 1>, deadline: u64, now: u64) {
    // SAFETY: retained owned storage, one modeled CPU, no hardware and a bounded clock.
    assert!(
        unsafe { timer.start_on_if_future(0.into(), instant(deadline), || now) }
            .unwrap()
            .is_some()
    );
}
fn deliver(now: u64) {
    // SAFETY: serialized modeled dispatch with all registered owners retained.
    assert!(unsafe { TIMERS.dispatch_due(0.into(), || now) }.unwrap());
}

impl Queue for Separate {
    fn prepare(n: usize, waker: &Waker) -> Self {
        Self {
            waits: (0..n)
                .map(|_| {
                    let state = Arc::new(state(waker));
                    (Timer::new(&TIMERS, SharedHandler(state.clone())), state)
                })
                .collect(),
        }
    }

    fn arm(&mut self, slot: usize, deadline: u64, now: u64) {
        arm(&self.waits[slot].0, deadline, now);
    }

    fn cancel(&mut self, slot: usize) {
        assert!(self.waits[slot].0.cancel_sync());
    }

    fn dispatch(&mut self, now: u64) {
        deliver(now);
    }
}
impl Queue for Embedded {
    fn prepare(n: usize, waker: &Waker) -> Self {
        Self {
            waits: (0..n)
                .map(|_| Timer::new(&TIMERS, EmbeddedHandler(state(waker))))
                .collect(),
        }
    }

    fn arm(&mut self, slot: usize, deadline: u64, now: u64) {
        arm(&self.waits[slot], deadline, now);
    }

    fn cancel(&mut self, slot: usize) {
        assert!(self.waits[slot].cancel_sync());
    }

    fn dispatch(&mut self, now: u64) {
        deliver(now);
    }
}

pub(super) fn run(events: usize) {
    assert!((4096..=1_048_576).contains(&events) && events.is_multiple_of(4096));
    println!("mode,version,case,n,round,operations,elapsed_ns,allocs,frees");
    for n in [1, 32, 256, 4096] {
        sample::<Separate>("lifecycle", n, 4096, false);
        sample::<Embedded>("lifecycle", n, 4096, false);
        for round in 0..3 {
            for version in ["separate", "embedded", "embedded", "separate"] {
                let (ns, ..) = if version == "separate" {
                    sample::<Separate>("lifecycle", n, events, false)
                } else {
                    sample::<Embedded>("lifecycle", n, events, false)
                };
                println!("timing,{version},lifecycle,{n},{round},{events},{ns},0,0");
            }
        }
        for version in ["separate", "embedded"] {
            let (ns, allocations, frees) = if version == "separate" {
                sample::<Separate>("lifecycle", n, events, true)
            } else {
                sample::<Embedded>("lifecycle", n, events, true)
            };
            let expected = events * if version == "separate" { 2 } else { 1 } + events / n;
            assert_eq!((allocations, frees), (expected, expected));
            println!("allocation,{version},lifecycle,{n},0,{events},{ns},{allocations},{frees}");
        }
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Generation-based subscriptions; no scheduler or platform recovery dependency.

use core::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use klazy::Once;
use kpoll::{PollEvent, PollRegisterError, PollRegistrations, PollSet};

/// Clock whose offset can change independently of monotonic time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClockDomain {
    /// Wall-clock adjustment and sleep injection affect this clock.
    Realtime,
    /// Only sleep injection affects this clock.
    Boottime,
}

struct Changes {
    realtime: PollEvent,
    boottime: PollEvent,
}
impl Changes {
    fn new() -> Self {
        Self {
            realtime: PollEvent::new(),
            boottime: PollEvent::new(),
        }
    }

    fn event(&self, clock: ClockDomain) -> &PollEvent {
        match clock {
            ClockDomain::Realtime => &self.realtime,
            ClockDomain::Boottime => &self.boottime,
        }
    }

    fn publish(&self, includes_boot: bool) -> (Option<PollSet>, Option<PollSet>) {
        (
            Some(self.realtime.notify_defer_wake()),
            includes_boot.then(|| self.boottime.notify_defer_wake()),
        )
    }
}
static CHANGES: Once<Changes> = Once::new();

/// Prepares a clock subscription source and samples its generation.
///
/// Call in task context before reading a deadline predicate. The first use
/// allocates sources. Generation wrap assumes no waiter spans usize::MAX updates.
fn clock_change_generation(clock: ClockDomain) -> usize {
    CHANGES.call_once(Changes::new).event(clock).generation()
}

/// Owned wait for a change to one clock's offset. Drop unregisters its waiter.
///
/// Prepare before reading the deadline. A change between construction and the
/// first poll is retained by the generation check. Poll only in task context.
#[must_use = "poll or await the clock-change listener"]
pub struct ClockChangeListener {
    clock: ClockDomain,
    observed: usize,
    registrations: PollRegistrations,
}

/// Prepares a change listener in task context, before sampling a deadline.
pub fn clock_change_listener(clock: ClockDomain) -> ClockChangeListener {
    ClockChangeListener {
        clock,
        observed: clock_change_generation(clock),
        registrations: PollRegistrations::new(),
    }
}
impl Future for ClockChangeListener {
    type Output = Result<(), PollRegisterError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        poll_change(
            CHANGES.call_once(Changes::new).event(this.clock),
            this.observed,
            &mut this.registrations,
            cx,
        )
    }
}

// Called with TIMEKEEPER held: no allocation or callbacks. An uninitialized
// source has no listeners; first-time registration precedes its predicate read.
pub(super) fn publish(includes_boot: bool) -> (Option<PollSet>, Option<PollSet>) {
    CHANGES
        .get()
        .map_or((None, None), |changes| changes.publish(includes_boot))
}
pub(super) fn wake(sources: (Option<PollSet>, Option<PollSet>)) {
    if let Some(source) = sources.0 {
        source.wake();
    }
    if let Some(source) = sources.1 {
        source.wake();
    }
}

fn poll_change(
    event: &PollEvent,
    observed: usize,
    registrations: &mut PollRegistrations,
    cx: &mut Context<'_>,
) -> Poll<Result<(), PollRegisterError>> {
    if event.has_changed_since(observed) {
        return Poll::Ready(Ok(()));
    }
    let mut context = registrations.context(cx);
    if let Err(error) = event.register(&mut context) {
        return Poll::Ready(Err(error));
    }
    drop(context);
    if event.has_changed_since(observed) {
        Poll::Ready(Ok(()))
    } else {
        Poll::Pending
    }
}

unittest::path_mod_test!(tests, "../ut-tests/change.rs");

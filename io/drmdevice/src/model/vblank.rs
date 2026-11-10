// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The vertical blanking clock of a card.
//!
//! Every vertical blanking fact userspace can observe comes from here: the
//! `WAIT_VBLANK` sequence and timestamp, the page-flip completion event, and
//! the point at which a scanout flush is considered on the wire. One source
//! advances the sequence, and it is the only thing that does:
//!
//! - the sequence advances exactly once per tick;
//! - the timestamp is the start of the new interval, so a client can measure
//!   `last + tstride` as the next expected blank;
//! - the tick work runs after the new state is published, so a page-flip event
//!   always carries the sequence of the blank it completed on, and a
//!   `WAIT_VBLANK` client released after that tick observes the same blank.
//!
//! Ticks are planned boundaries derived from the refresh rate, not a latch
//! observed on the display wire: the bound display driver reports no scanout
//! interrupt, so the timestamps are exact boundaries of a timeline, not
//! measured scanout events. The card therefore does not advertise
//! `DRM_CAP_CRTC_IN_VBLANK_EVENT`.

use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    sync::atomic::{AtomicBool, AtomicU32},
    task::Waker,
};

use kspin::SpinNoIrq;
use ktime_types::{MonotonicInstant, TimeSpan};

use super::device::DrmDevice;

/// The per-tick work the clock runs after publishing a tick.
///
/// Implemented by the card object so the clock can complete flushes and page
/// flips without knowing about framebuffers or events.
pub(crate) trait TickHandler: Send + Sync {
    /// Runs once per tick, with the clock's state lock released, and releases
    /// the waiters whose blank this tick completed.
    ///
    /// May run in timer interrupt context and must not sleep.
    fn on_tick(&self);
}

/// The published state of one clock.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TickState {
    /// Wrapping DRM sequence number of the most recent blank.
    pub(crate) sequence: u32,
    /// Start instant of the interval that began with the most recent blank.
    pub(crate) timestamp: TimeSpan,
}

impl TickState {
    /// The state of a blank the display reported.
    pub(crate) const fn at(sequence: u32, timestamp: TimeSpan) -> Self {
        Self {
            sequence,
            timestamp,
        }
    }
}

/// Tracks where a client's requests are relative to its last observation.
///
/// Every decision here is about *distances*: how many blanks a value lies
/// after another one. Distances are the only quantities that compare
/// meaningfully in a wrapping sequence domain, so the file remembers one
/// number, the distance of its cursor behind the newest blank, and each
/// request contributes one more distance. The file has no cursor before its
/// first request; it counts from the newest blank then.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SequenceTracker {
    /// Distance from the newest blank to the file's cursor.
    cursor_behind: Option<u32>,
}

impl SequenceTracker {
    /// The newest blank's distance from the file's cursor; zero before the
    /// file observed anything.
    pub(crate) const fn behind(&self) -> u32 {
        match self.cursor_behind {
            Some(behind) => behind,
            None => 0,
        }
    }

    /// Records the cursor after the file requested `behind` blanks past the
    /// newest one.
    pub(crate) fn advance(&mut self, behind: u32) {
        self.cursor_behind = Some(behind);
    }
}

/// One `WAIT_VBLANK` request resolved against the clock.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VblankRequest {
    /// A blank this client already observed: answered immediately.
    Past,
    /// The blank following the newest one, at most one interval away.
    AtNext,
    /// The blank `distance` intervals after the newest one, which is at least
    /// two.
    Ahead(u32),
}

impl VblankRequest {
    /// Intervals this request waits for, counted from the newest blank.
    ///
    /// `Past` waits for nothing; both waiting forms count at least one.
    pub(crate) const fn intervals(self) -> u32 {
        match self {
            Self::Past => 0,
            Self::AtNext => 1,
            Self::Ahead(distance) => distance,
        }
    }

    /// The monotonic sequence value this request completes on.
    pub(crate) fn target(self, sequence: u32) -> u64 {
        u64::from(sequence).saturating_add(u64::from(self.intervals()))
    }
}

/// The distance past the newest blank that a request names.
///
/// `requested` is the request's distance from the file's cursor and `behind`
/// the newest blank's distance from the same cursor, so the target is
/// `requested - behind`. A relative request never targets a blank earlier than
/// the one it counts from, so a cursor that has already passed the requested
/// distance resolves to the next blank instead of wrapping.
pub(crate) fn distance_past_current(requested: u32, behind: u32) -> u32 {
    requested.saturating_sub(behind)
}

/// Classifies a distance past the newest blank.
pub(crate) const fn classify(distance: u32) -> VblankRequest {
    match distance {
        0 | 1 => VblankRequest::AtNext,
        distance => VblankRequest::Ahead(distance),
    }
}

/// Resolves a relative request against the clock.
///
/// The request is a number of intervals counted from the file's cursor. The
/// file advances its cursor to the blank the request names, so a later request
/// counts from there, exactly as Linux's per-file `vblank->last` does.
pub(crate) fn resolve_relative(
    tracker: &mut SequenceTracker,
    _current: u32,
    requested: u32,
) -> VblankRequest {
    let distance = distance_past_current(requested, tracker.behind());
    tracker.advance(distance);
    classify(distance)
}

/// Resolves an absolute request against the clock.
///
/// The client names a blank; the request's distance from the newest blank is
/// what decides between past and future. The current blank and values behind
/// it are `Past`; one interval further is `AtNext`; anything else is `Ahead`.
pub(crate) fn resolve_absolute(
    tracker: &mut SequenceTracker,
    current: u32,
    requested: u32,
) -> VblankRequest {
    let _ = tracker;
    // The client named a blank; both distances below come from the same pair,
    // but only one of them says whether the named blank is later than the
    // newest one. That distinction is made by the sequence comparison the
    // clock's own wrap handling uses, not by the subtraction direction.
    let ahead = requested.wrapping_sub(current);
    if !is_in_future(current, requested) {
        return VblankRequest::Past;
    }
    match ahead {
        1 => VblankRequest::AtNext,
        ahead => VblankRequest::Ahead(ahead),
    }
}

/// Whether `later` comes after `earlier` in one wrap of the sequence domain.
///
/// The sequence advances by one per blank, so the forward distance from
/// `earlier` to `later` is a small number and the backward one is close to a
/// full wrap. Comparing the two distances against the length of the domain is
/// done on the raw values only after each has been subtracted from its own
/// start, which keeps the answer in the domain the caller asked about.
fn is_in_future(earlier: u32, later: u32) -> bool {
    let forward = later.wrapping_sub(earlier);
    let backward = earlier.wrapping_sub(later);
    forward != 0 && forward.wrapping_add(1) != 0 && forward != backward && is_short(forward)
}

/// Whether a distance is short enough to be a forward gap.
///
/// A blank interval is never more than a few thousand apart; anything near the
/// top of the domain is the same value seen from the other side of the wrap.
fn is_short(distance: u32) -> bool {
    // `min(dist, MAX - dist)` turns "near the top" into "near zero" by a
    // subtraction, and the result is compared only against a small count.
    let near_top = u32::MAX.checked_sub(distance);
    match near_top {
        Some(near_top) => near_top.checked_sub(distance).is_some(),
        None => false,
    }
}

/// One task waiting for a blank.
///
/// The handle is registered before the waiting task is parked and withdrawn
/// when it returns, so a wake that lands between the state check and the park
/// is kept by the task's own registration instead of being lost.
pub(crate) struct ParkWaiter {
    target: u64,
    wake: Waker,
}

/// One card's vertical blanking clock.
///
/// The published timeline is protected by a spinlock the timer interrupt takes,
/// so every observer sees one sequence. The waiter list shares that lock only
/// to enqueue and dequeue a handle; waiters are woken after it is released, and
/// the tick handler runs after the lock is released too, so nothing in the tick
/// path holds the clock lock while it pushes pixels or publishes events.
pub(crate) struct VblankClockState {
    tick: SpinNoIrq<TickState>,
    waiters: SpinNoIrq<Vec<ParkWaiter>>,
}

impl VblankClockState {
    /// Creates a stopped clock.
    pub(crate) fn new() -> Self {
        Self {
            tick: SpinNoIrq::new(TickState::default()),
            waiters: SpinNoIrq::new(Vec::new()),
        }
    }

    /// The published timeline.
    pub(crate) fn tick(&self) -> TickState {
        *self.tick.lock()
    }

    /// Publishes one blank and runs the tick work that completes on it.
    ///
    /// The timeline becomes visible first, so a client released by the tick
    /// work observes the blank its completion names.
    pub(crate) fn report(
        &self,
        sequence: u32,
        timestamp: MonotonicInstant,
        handler: &dyn TickHandler,
    ) {
        *self.tick.lock() = TickState::at(sequence, timestamp.span_since_origin());
        handler.on_tick();
    }

    /// Registers a task wait for the blank after `target`.
    ///
    /// Registering the same waker again replaces the earlier entry, so a task
    /// that re-parks never accumulates handles.
    pub(crate) fn park(&self, target: u64, wake: &Waker) {
        let mut waiters = self.waiters.lock();
        waiters.retain(|waiter| !waiter.wake.will_wake(wake));
        waiters.push(ParkWaiter {
            target,
            wake: wake.clone(),
        });
    }

    /// Withdraws a task wait.
    ///
    /// A wake that already left the list before this call is harmless: the
    /// waiting task rechecks its state before parking.
    pub(crate) fn unpark(&self, wake: &Waker) {
        self.waiters
            .lock()
            .retain(|waiter| !waiter.wake.will_wake(wake));
    }

    /// Releases every waiter so it can observe a vanished display.
    ///
    /// The card stops its clock before calling this, so no later blank can
    /// re-register a waiter on a display that no longer exists.
    pub(crate) fn remove(&self) {
        self.wake_all();
    }

    /// Wakes every registered waiter without completing a blank.
    fn wake_all(&self) {
        let mut ready = Vec::new();
        {
            let mut waiters = self.waiters.lock();
            ready.extend(waiters.drain(..).map(|waiter| waiter.wake));
        }
        for wake in ready {
            wake.wake_by_ref();
        }
    }

    /// Whether the blank numbered `target` has completed.
    ///
    /// `target` is the absolute sequence value from [`VblankRequest::target`].
    ///
    /// The timeline is a wrapping 32-bit counter: a target has been reached
    /// once the published sequence is at or past it. In wrapping arithmetic
    /// that is a zero distance or one above half the range.
    pub(crate) fn reached(&self, target: u64) -> bool {
        let sequence = self.tick.lock().sequence;
        let distance = (target as u32).wrapping_sub(sequence);
        distance == 0 || distance > u32::MAX / 2
    }

    /// The blank and timestamp a client observes now.
    pub(crate) fn tick_report(&self) -> (u32, TimeSpan) {
        let tick = self.tick.lock();
        (tick.sequence, tick.timestamp)
    }

    /// Releases every waiter whose blank has completed, and drops it.
    ///
    /// Called by the tick work after the flips of this blank were completed.
    pub(crate) fn release_waiters(&self) {
        let sequence = self.tick.lock().sequence;
        let mut ready = Vec::new();
        {
            let mut waiters = self.waiters.lock();
            waiters.retain(|waiter| {
                if sequence >= waiter.target as u32 {
                    ready.push(waiter.wake.clone());
                    false
                } else {
                    true
                }
            });
        }
        for wake in ready {
            wake.wake_by_ref();
        }
    }
}

impl Drop for VblankClockState {
    fn drop(&mut self) {
        // Nothing will tick again; waiters still parked must return instead of
        // waiting for a blank that cannot happen.
        for waiter in core::mem::take(self.waiters.get_mut()) {
            waiter.wake.wake_by_ref();
        }
    }
}

/// The vertical blanking state of one card.
///
/// The shared timeline, whether the display's own source drives it, and the
/// tick work the card runs on every blank.
pub(crate) struct VblankState {
    /// The shared timeline.
    pub(crate) clock: Arc<VblankClockState>,
    /// Whether the display's own source drives the clock.
    pub(crate) scanout_started: AtomicBool,
    /// Whether the timer fallback is currently driving the clock.
    ///
    /// Linux's virtio-gpu has no blank interrupt and uses
    /// `DRM_CRTC_VBLANK_TIMER_FUNCS`: a high-resolution timer ticks the
    /// timeline at the mode's refresh rate while the CRTC is enabled.
    pub(crate) timer_on: AtomicBool,
    /// Whether the fallback thread was started; it runs for the card's life.
    pub(crate) timer_started: AtomicBool,
    /// Refresh rate the fallback ticks at, in hertz.
    pub(crate) refresh_hz: AtomicU32,
    /// The tick handler the card runs on every blank.
    ticks: Arc<DeviceTicks>,
}

impl VblankState {
    /// Creates the timeline of one card.
    ///
    /// The tick work reaches the card through `device`, so it can be created
    /// while the card itself is still being built.
    pub(crate) fn new(device: &Weak<DrmDevice>) -> Self {
        Self {
            clock: Arc::new(VblankClockState::new()),
            scanout_started: AtomicBool::new(false),
            timer_on: AtomicBool::new(false),
            timer_started: AtomicBool::new(false),
            refresh_hz: AtomicU32::new(60),
            ticks: Arc::new(DeviceTicks {
                device: device.clone(),
            }),
        }
    }

    /// The card's tick work.
    pub(crate) fn ticks(&self) -> Arc<DeviceTicks> {
        self.ticks.clone()
    }
}

/// The tick work of one card.
///
/// A tick publishes the card's new blank first, then `on_tick` completes the
/// page flips that latched with it, and only then are `WAIT_VBLANK` waiters
/// released: a client woken by the tick observes the same blank its flip
/// completed on.
///
/// The handler reaches the clock through a weak reference to the card, so it
/// can be registered before the card exists and never outlives it.
pub(crate) struct DeviceTicks {
    device: Weak<DrmDevice>,
}

impl DeviceTicks {
    /// Feeds one blank the display reported into the card's timeline.
    ///
    /// The display owns the sequence it reports; the card publishes it as
    /// reported and runs the tick work that completes on it.
    pub(crate) fn edge(&self, tick: display::VblankTick) {
        let Some(device) = self.device.upgrade() else {
            return;
        };
        device.vblank.clock.report(
            tick.sequence,
            tick.timestamp,
            device.vblank.ticks().as_ref(),
        );
    }
}

impl TickHandler for DeviceTicks {
    fn on_tick(&self) {
        let Some(device) = self.device.upgrade() else {
            // The card is gone; a waiter left in the clock is released by the
            // clock's own drop.
            return;
        };
        device.complete_flips();
        device.vblank.clock.release_waiters();
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! System timekeeping for X-Kernel.
//!
//! `ktime` is the system timekeeping owner: it derives realtime from the
//! monotonic hardware clock exposed by [`khal::time`] plus a clock
//! correlation, and owns cumulative sleep time, initialization, updates, and reads.
//! `khal` stays a pure hardware time source; the `drivers/platform/rtc` crate only
//! provides persistent-clock samples that boot code passes to
//! [`initialize_realtime`].
//!
//! Semantics:
//! - realtime advances 1:1 with monotonic time between explicit updates;
//! - boottime is monotonic plus cumulative injected sleep time;
//! - [`inject_sleeptime`] advances realtime and boottime without changing monotonic;
//! - [`begin_suspend_accounting`] and [`resume_suspend_accounting`] provide a
//!   single-use accounting transaction, without suspending clocks or hardware;
//! - before any persistent clock is sampled, realtime falls back to the Unix
//!   epoch plus elapsed monotonic time and injected sleep intervals;
//! - [`set_realtime_checked`] re-bases the correlation so a caller-supplied
//!   wall-clock timestamp takes effect immediately. Range validation (the Linux
//!   rule that the wall clock may not move before `CLOCK_MONOTONIC`, plus an
//!   upper bound keeping the clock able to advance) is enforced atomically
//!   inside the timekeeper, so callers only need to perform permission checks.

#![no_std]
#![deny(unsafe_code)]

extern crate alloc;

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use khal::time::monotonic_time;
use kspin::SpinRwNoIrq;

mod change;
mod observer;
mod suspend;
pub use change::{ClockChangeListener, ClockDomain, clock_change_listener};
use ktime_types::{BoottimeInstant, MonotonicInstant, NANOS_PER_SEC, SystemTime, TimeSpan};
pub use observer::{
    ClockChangeId, ClockObserver, ClockObserverRegistration, ClockReading, ClockUpdate,
    register_clock_observer,
};
pub use suspend::{
    SuspendAccountingError, SuspendToken, begin_suspend_accounting, cancel_suspend_accounting,
    resume_suspend_accounting,
};

/// Largest settable wall-clock second value, mirroring Linux `KTIME_SEC_MAX`
/// (`include/linux/time64.h`): the upper bound `timespec64_valid()` accepts.
///
/// Values at or beyond this would make the monotonic-to-realtime addition in
/// [`ClockCorrelation::realtime_at`] saturate at [`SystemTime::MAX`] on the
/// next read, freezing the wall clock. The wide margin below [`i64::MAX`]
/// guarantees the clock can keep advancing.
const MAX_SETTABLE_UNIX_SECONDS: i64 = i64::MAX / NANOS_PER_SEC as i64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ClockCorrelation {
    monotonic_base: MonotonicInstant,
    realtime_base: SystemTime,
}

impl ClockCorrelation {
    const FALLBACK: Self = Self {
        monotonic_base: MonotonicInstant::ORIGIN,
        realtime_base: SystemTime::UNIX_EPOCH,
    };

    const fn new(monotonic_base: MonotonicInstant, realtime_base: SystemTime) -> Self {
        Self {
            monotonic_base,
            realtime_base,
        }
    }

    #[inline]
    fn realtime_at(self, monotonic_now: MonotonicInstant) -> SystemTime {
        let elapsed = monotonic_now
            .checked_duration_since(self.monotonic_base)
            .unwrap_or(TimeSpan::ZERO);
        self.realtime_base
            .checked_add(elapsed)
            .unwrap_or(SystemTime::MAX)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Timekeeper {
    correlation: ClockCorrelation,
    sleep_time: TimeSpan,
    suspend: suspend::SuspendState,
}

impl Timekeeper {
    const INITIAL: Self = Self {
        correlation: ClockCorrelation::FALLBACK,
        sleep_time: TimeSpan::ZERO,
        suspend: suspend::SuspendState::INITIAL,
    };

    fn snapshot(self, monotonic: MonotonicInstant) -> ClockSnapshot {
        ClockSnapshot {
            monotonic,
            realtime: self.correlation.realtime_at(monotonic),
            boottime: BoottimeInstant::from_span_since_origin(
                monotonic
                    .span_since_origin()
                    .saturating_add(self.sleep_time),
            ),
        }
    }

    fn set_realtime_checked(
        &mut self,
        sample: SystemTime,
        monotonic: MonotonicInstant,
    ) -> Result<(), RealtimeOutOfRange> {
        let min_allowed = SystemTime::UNIX_EPOCH
            .checked_add(monotonic.span_since_origin())
            .ok_or(RealtimeOutOfRange)?;
        if sample < min_allowed || sample.unix_seconds() >= MAX_SETTABLE_UNIX_SECONDS {
            return Err(RealtimeOutOfRange);
        }
        self.correlation = ClockCorrelation::new(monotonic, sample);
        Ok(())
    }

    fn inject_sleeptime(
        &mut self,
        delta: TimeSpan,
        monotonic: MonotonicInstant,
    ) -> Result<(), SleepTimeError> {
        if delta.is_zero() {
            return Ok(());
        }
        if self.suspend.is_active() {
            return Err(SleepTimeError::AccountingInProgress);
        }
        self.apply_sleeptime(delta, monotonic)
    }

    // Arithmetic shared by untracked injection and validated transaction commit.
    fn apply_sleeptime(
        &mut self,
        delta: TimeSpan,
        monotonic: MonotonicInstant,
    ) -> Result<(), SleepTimeError> {
        if delta.is_zero() {
            return Ok(());
        }
        let sleep_time = self
            .sleep_time
            .checked_add(delta)
            .ok_or(SleepTimeError::OutOfRange)?;
        // Reject an unrepresentable boottime before publishing either offset.
        monotonic
            .span_since_origin()
            .checked_add(sleep_time)
            .ok_or(SleepTimeError::OutOfRange)?;
        let elapsed = monotonic
            .checked_duration_since(self.correlation.monotonic_base)
            .ok_or(SleepTimeError::InvalidMonotonicBaseline)?;
        let realtime = self
            .correlation
            .realtime_base
            .checked_add(elapsed)
            .and_then(|now| now.checked_add(delta))
            .ok_or(SleepTimeError::OutOfRange)?;
        if realtime.unix_seconds() >= MAX_SETTABLE_UNIX_SECONDS {
            return Err(SleepTimeError::OutOfRange);
        }
        // Validation is complete; commit the two clock offsets together.
        self.correlation = ClockCorrelation::new(monotonic, realtime);
        self.sleep_time = sleep_time;
        Ok(())
    }
}

static TIMEKEEPER: SpinRwNoIrq<Timekeeper> = SpinRwNoIrq::new(Timekeeper::INITIAL);
static CHANGE_ID: AtomicU64 = AtomicU64::new(0);
static REALTIME_INITIALIZED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ClockSnapshot {
    monotonic: MonotonicInstant,
    realtime: SystemTime,
    boottime: BoottimeInstant,
}

/// Initializes realtime from a persistent-clock sample.
///
/// The first sample establishes the clock correlation. Later calls leave the
/// established correlation unchanged, so platform code may safely use this as
/// an idempotent early-boot operation.
pub fn initialize_realtime(sample: SystemTime) {
    if REALTIME_INITIALIZED.load(Ordering::Acquire) {
        return;
    }
    let mut timekeeper = TIMEKEEPER.write();
    if !REALTIME_INITIALIZED.load(Ordering::Relaxed) {
        // Sample monotonic while holding the write lock so the recorded base
        // matches the correlation we publish.
        let monotonic_base = monotonic_time();
        timekeeper.correlation = ClockCorrelation::new(monotonic_base, sample);
        REALTIME_INITIALIZED.store(true, Ordering::Release);
        let update = prepare_clock_update(&timekeeper, false);
        let wake = change::publish(false);
        drop(timekeeper);
        observer::notify(update);
        change::wake(wake);
    }
}

/// The requested wall-clock value is outside the settable range.
///
/// Returned by [`set_realtime_checked`] when the value would move the wall
/// clock before `CLOCK_MONOTONIC` (Linux 4.3+ rule) or beyond
/// [`MAX_SETTABLE_UNIX_SECONDS`] (Linux `KTIME_SEC_MAX`).
#[derive(Debug)]
pub struct RealtimeOutOfRange;

/// Validates and applies a wall-clock update atomically.
///
/// Range validation and the correlation update both happen under the same
/// write lock using a single monotonic sample, so:
///
/// - a concurrent reader can never observe a torn [`ClockSnapshot`] whose
///   `realtime` was derived from a correlation written *after* its `monotonic`
///   was sampled;
/// - the Linux `wall_to_monotonic` constraint (`realtime >=
///   UNIX_EPOCH + monotonic`) cannot be violated by a gap between validation
///   and commit.
///
/// Syscall layers keep only the privilege check and map the error to `EINVAL`.
///
/// # Errors
///
/// Returns [`RealtimeOutOfRange`] when `sample` would move the wall clock
/// before `CLOCK_MONOTONIC` or beyond [`MAX_SETTABLE_UNIX_SECONDS`]
/// (Linux `KTIME_SEC_MAX`).
pub fn set_realtime_checked(sample: SystemTime) -> Result<(), RealtimeOutOfRange> {
    let mut timekeeper = TIMEKEEPER.write();
    // Sample monotonic under the write lock so validation and the recorded
    // base observe the same instant.
    let monotonic_base = monotonic_time();

    timekeeper.set_realtime_checked(sample, monotonic_base)?;
    REALTIME_INITIALIZED.store(true, Ordering::Release);
    let update = prepare_clock_update(&timekeeper, false);
    let wake = change::publish(false);
    drop(timekeeper);
    observer::notify(update);
    change::wake(wake);
    Ok(())
}

#[inline]
fn snapshot() -> ClockSnapshot {
    // Hold the read lock across the monotonic sample and the realtime
    // derivation so a concurrent writer cannot publish a new correlation in
    // between and produce a torn (monotonic, realtime) pair.
    let timekeeper = TIMEKEEPER.read();
    timekeeper.snapshot(monotonic_time())
}

/// Samples realtime and monotonic time at the same instant.
///
/// Both values use one monotonic sample under the timekeeper read lock, so
/// concurrent wall-clock updates cannot tear the pair. The owned timestamps
/// remain valid after later updates. This does not sleep or require a process
/// context and is also valid in interrupt context and during early boot.
#[inline]
pub fn realtime_and_monotonic() -> (SystemTime, MonotonicInstant) {
    let now = snapshot();
    (now.realtime, now.monotonic)
}

/// Returns the current realtime timestamp.
///
/// Before a persistent clock initializes realtime, the result falls back to
/// the Unix epoch plus elapsed monotonic time and injected sleep intervals.
#[inline]
pub fn realtime() -> SystemTime {
    snapshot().realtime
}

/// Returns the current boot-time instant, including injected sleep intervals.
///
/// Uses a single monotonic sample under the timekeeper read lock. This does not
/// sleep and is valid in interrupt context and during early boot. Before any
/// sleep interval is injected, its elapsed value equals monotonic time.
#[inline]
pub fn boottime() -> BoottimeInstant {
    snapshot().boottime
}

/// The reason a confirmed sleep interval could not be applied.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SleepTimeError {
    /// A nonzero untracked injection would bypass an active suspend transaction.
    AccountingInProgress,
    /// Cumulative sleep time or a clock value would exceed its supported range.
    OutOfRange,
    /// The monotonic sample precedes the recorded realtime correlation base.
    ///
    /// The caller must resolve the counter/baseline inconsistency before retrying;
    /// reducing the sleep interval does not repair this condition.
    InvalidMonotonicBaseline,
}

/// Adds a confirmed sleep interval to realtime and boottime atomically.
///
/// Monotonic time is unchanged. `TimeSpan` excludes negative intervals, and a
/// zero interval is a successful no-op. This does not sleep and is valid in
/// interrupt context; it performs no device reads. Registered wakers must be
/// IRQ-safe. Clock-change notifications run after releasing the timekeeper lock.
///
/// The caller must establish a suspend/resume boundary, exclude `delta` from
/// the underlying monotonic clock, and ensure each interval is injected once.
/// This function neither detects sleep nor repairs hardware counters. The
/// registered consumers recheck their original deadlines after notification.
/// No production resume path is wired to this interface yet.
///
/// # Errors
///
/// Returns [`SleepTimeError::OutOfRange`] if either clock or cumulative sleep
/// time cannot represent the update, or realtime would reach the upper bound
/// enforced by [`set_realtime_checked`]. Returns
/// [`SleepTimeError::InvalidMonotonicBaseline`] if the monotonic sample precedes
/// the recorded correlation base. Returns [`SleepTimeError::AccountingInProgress`]
/// for a nonzero injection while a suspend accounting transaction is active.
/// Every error leaves state unchanged.
pub fn inject_sleeptime(delta: TimeSpan) -> Result<(), SleepTimeError> {
    if delta.is_zero() {
        return Ok(());
    }
    let mut timekeeper = TIMEKEEPER.write();
    timekeeper.inject_sleeptime(delta, monotonic_time())?;
    let update = prepare_clock_update(&timekeeper, true);
    let wake = change::publish(true);
    drop(timekeeper);
    observer::notify(update);
    change::wake(wake);
    Ok(())
}

/// Converts a realtime deadline into the monotonic timer domain.
///
/// Deadlines that have already passed map to the current monotonic instant.
/// Unrepresentable future deadlines saturate at the largest monotonic instant.
#[inline]
pub fn realtime_deadline_to_monotonic(deadline: SystemTime) -> MonotonicInstant {
    let now = snapshot();
    let delay = deadline
        .duration_since(now.realtime)
        .unwrap_or(TimeSpan::ZERO);
    now.monotonic
        .checked_add(delay)
        .unwrap_or(MonotonicInstant::from_span_since_origin(TimeSpan::MAX))
}

/// Converts a boot-time deadline to a snapshot of the monotonic timer domain.
///
/// Recompute after a clock-change notification; this conversion does not retain
/// a subscription. Past deadlines map to the current monotonic instant.
pub fn boottime_deadline_to_monotonic(deadline: BoottimeInstant) -> MonotonicInstant {
    let now = snapshot();
    let delay = deadline.saturating_duration_since(now.boottime);
    now.monotonic
        .checked_add(delay)
        .unwrap_or(MonotonicInstant::from_span_since_origin(TimeSpan::MAX))
}

unittest::path_mod_test!(tests, "../ut-tests/lib.rs");

fn prepare_clock_update(timekeeper: &Timekeeper, includes_boottime: bool) -> ClockUpdate {
    // Serialized by TIMEKEEPER; readers sample this identity under its read lock.
    let id = CHANGE_ID.load(Ordering::Relaxed).wrapping_add(1);
    CHANGE_ID.store(id, Ordering::Relaxed);
    let sample = timekeeper.snapshot(monotonic_time());
    ClockUpdate {
        reading: ClockReading::new(sample, id),
        includes_boottime,
    }
}

/// Reads all three clock domains and the last committed change identity together.
/// Usable in IRQ context; does not allocate or initialize notification sources.
pub fn clock_reading() -> ClockReading {
    let timekeeper = TIMEKEEPER.read();
    ClockReading::new(
        timekeeper.snapshot(monotonic_time()),
        CHANGE_ID.load(Ordering::Relaxed),
    )
}

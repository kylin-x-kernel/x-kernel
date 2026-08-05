// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! System timekeeping for X-Kernel.
//!
//! `ktime` is the system timekeeping owner: it derives realtime from the
//! monotonic hardware clock exposed by [`khal::time`] plus a clock
//! correlation, and owns initialization, runtime updates, and reads. `khal`
//! stays a pure hardware time source; [`drivers::rtc`] only provides
//! persistent-clock samples that boot code passes to
//! [`initialize_realtime`].
//!
//! Semantics:
//! - realtime advances 1:1 with monotonic time after initialization;
//! - before any persistent clock is sampled, realtime falls back to the Unix
//!   epoch plus elapsed monotonic time;
//! - [`set_realtime_checked`] re-bases the correlation so a caller-supplied
//!   wall-clock timestamp takes effect immediately. Range validation (the Linux
//!   rule that the wall clock may not move before `CLOCK_MONOTONIC`, plus an
//!   upper bound keeping the clock able to advance) is enforced atomically
//!   inside the timekeeper, so callers only need to perform permission checks.

#![no_std]
#![deny(unsafe_code)]

use core::sync::atomic::{AtomicBool, Ordering};

use khal::time::monotonic_time;
use kspin::SpinRwNoIrq;
use ktime_types::{MonotonicInstant, NANOS_PER_SEC, SystemTime, TimeSpan};

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

static REALTIME_CORRELATION: SpinRwNoIrq<ClockCorrelation> =
    SpinRwNoIrq::new(ClockCorrelation::FALLBACK);
static REALTIME_INITIALIZED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ClockSnapshot {
    monotonic: MonotonicInstant,
    realtime: SystemTime,
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
    let mut correlation = REALTIME_CORRELATION.write();
    if !REALTIME_INITIALIZED.load(Ordering::Relaxed) {
        // Sample monotonic while holding the write lock so the recorded base
        // matches the correlation we publish.
        let monotonic_base = monotonic_time();
        *correlation = ClockCorrelation::new(monotonic_base, sample);
        REALTIME_INITIALIZED.store(true, Ordering::Release);
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
/// Returns [`Err(RealtimeOutOfRange)`] if `sample` would move the wall clock
/// before `CLOCK_MONOTONIC` or beyond the upper bound. Syscall layers keep only
/// the privilege check and map this error to `EINVAL`.
pub fn set_realtime_checked(sample: SystemTime) -> Result<(), RealtimeOutOfRange> {
    let mut correlation = REALTIME_CORRELATION.write();
    // Sample monotonic under the write lock so validation and the recorded
    // base observe the same instant.
    let monotonic_base = monotonic_time();

    let min_allowed = SystemTime::UNIX_EPOCH
        .checked_add(monotonic_base.span_since_origin())
        .ok_or(RealtimeOutOfRange)?;
    if sample < min_allowed {
        return Err(RealtimeOutOfRange);
    }
    if sample.unix_seconds() >= MAX_SETTABLE_UNIX_SECONDS {
        return Err(RealtimeOutOfRange);
    }

    *correlation = ClockCorrelation::new(monotonic_base, sample);
    REALTIME_INITIALIZED.store(true, Ordering::Release);
    Ok(())
}

#[inline]
fn snapshot() -> ClockSnapshot {
    // Hold the read lock across the monotonic sample and the realtime
    // derivation so a concurrent writer cannot publish a new correlation in
    // between and produce a torn (monotonic, realtime) pair.
    let correlation = REALTIME_CORRELATION.read();
    let monotonic = monotonic_time();
    ClockSnapshot {
        monotonic,
        realtime: correlation.realtime_at(monotonic),
    }
}

/// Returns the current realtime timestamp.
///
/// Before a persistent clock initializes realtime, the result falls back to
/// the Unix epoch plus elapsed monotonic time.
#[inline]
pub fn realtime() -> SystemTime {
    snapshot().realtime
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

#[cfg(unittest)]
mod tests {
    use unittest::{assert_eq, def_test};

    use super::*;

    #[def_test]
    fn correlation_preserves_elapsed_time() {
        let correlation = ClockCorrelation::new(
            MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(4)),
            SystemTime::from_unix_seconds(1_000),
        );
        assert_eq!(
            correlation.realtime_at(MonotonicInstant::from_span_since_origin(
                TimeSpan::from_secs(7)
            )),
            SystemTime::from_unix_seconds(1_003)
        );
    }

    #[def_test]
    fn correlation_supports_pre_epoch_realtime() {
        let correlation = ClockCorrelation::new(
            MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(10)),
            SystemTime::from_unix_seconds(-5),
        );
        assert_eq!(
            correlation.realtime_at(MonotonicInstant::from_span_since_origin(
                TimeSpan::from_secs(12)
            )),
            SystemTime::from_unix_seconds(-3)
        );
    }

    #[def_test]
    fn fallback_starts_at_unix_epoch() {
        assert_eq!(
            ClockCorrelation::FALLBACK.realtime_at(MonotonicInstant::from_span_since_origin(
                TimeSpan::from_secs(3)
            )),
            SystemTime::from_unix_seconds(3)
        );
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, def_test};

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

#[def_test]
fn sleep_injection_preserves_monotonic_and_advances_other_clocks() {
    let mut timekeeper = Timekeeper::INITIAL;
    let monotonic = MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(100));
    assert!(
        timekeeper
            .set_realtime_checked(SystemTime::from_unix_seconds(1_000), monotonic)
            .is_ok()
    );
    let before = timekeeper.snapshot(monotonic);
    assert!(
        timekeeper
            .inject_sleeptime(TimeSpan::from_secs(30), monotonic)
            .is_ok()
    );
    let after = timekeeper.snapshot(monotonic);
    assert_eq!(after.monotonic, before.monotonic);
    assert_eq!(after.realtime, SystemTime::from_unix_seconds(1_030));
    assert_eq!(after.boottime.span_since_origin(), TimeSpan::from_secs(130));
    let later = timekeeper.snapshot(monotonic + TimeSpan::from_secs(5));
    assert_eq!(later.realtime, SystemTime::from_unix_seconds(1_035));
    assert_eq!(later.boottime.span_since_origin(), TimeSpan::from_secs(135));
    // An owned snapshot remains stable across a later update.
    assert_eq!(before.realtime, SystemTime::from_unix_seconds(1_000));
    assert_eq!(
        before.boottime.span_since_origin(),
        TimeSpan::from_secs(100)
    );
}

#[def_test]
fn repeated_sleep_injection_accumulates_fractional_intervals() {
    let mut timekeeper = Timekeeper::INITIAL;
    let start = MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(10));
    assert!(
        timekeeper
            .inject_sleeptime(TimeSpan::from_millis(750), start)
            .is_ok()
    );
    let later = start + TimeSpan::from_secs(2);
    assert!(
        timekeeper
            .inject_sleeptime(TimeSpan::from_millis(500), later)
            .is_ok()
    );
    let now = timekeeper.snapshot(later);
    assert_eq!(timekeeper.sleep_time, TimeSpan::from_millis(1_250));
    assert_eq!(now.monotonic, later);
    assert_eq!(
        now.boottime.span_since_origin(),
        TimeSpan::from_millis(13_250)
    );
    assert_eq!(
        now.realtime.duration_since(SystemTime::UNIX_EPOCH),
        Ok(TimeSpan::from_millis(13_250))
    );
}

#[def_test]
fn zero_sleep_injection_leaves_state_unchanged() {
    let mut timekeeper = Timekeeper::INITIAL;
    let before = timekeeper;
    assert!(
        timekeeper
            .inject_sleeptime(TimeSpan::ZERO, MonotonicInstant::ORIGIN)
            .is_ok()
    );
    assert_eq!(timekeeper, before);
}

#[def_test]
fn realtime_update_preserves_sleep_offset() {
    let mut timekeeper = Timekeeper::INITIAL;
    let monotonic = MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(100));
    assert!(
        timekeeper
            .inject_sleeptime(TimeSpan::from_secs(30), monotonic)
            .is_ok()
    );
    let before = timekeeper.snapshot(monotonic);
    assert!(
        timekeeper
            .set_realtime_checked(SystemTime::from_unix_seconds(500), monotonic)
            .is_ok()
    );
    let after = timekeeper.snapshot(monotonic);
    assert_eq!(after.monotonic, before.monotonic);
    assert_eq!(after.boottime, before.boottime);
    assert_eq!(after.realtime, SystemTime::from_unix_seconds(500));
    let state = timekeeper;
    assert!(
        timekeeper
            .set_realtime_checked(SystemTime::from_unix_seconds(99), monotonic)
            .is_err()
    );
    assert_eq!(timekeeper, state);
}

#[def_test]
fn sleep_injection_rejects_realtime_bound_without_partial_update() {
    let mut timekeeper = Timekeeper::INITIAL;
    assert!(
        timekeeper
            .set_realtime_checked(
                SystemTime::from_unix_seconds(MAX_SETTABLE_UNIX_SECONDS - 1),
                MonotonicInstant::ORIGIN,
            )
            .is_ok()
    );
    let before = timekeeper;
    assert_eq!(
        timekeeper.inject_sleeptime(TimeSpan::from_secs(1), MonotonicInstant::ORIGIN),
        Err(SleepTimeError::OutOfRange)
    );
    assert_eq!(timekeeper, before);
    assert_eq!(
        timekeeper.inject_sleeptime(TimeSpan::MAX, MonotonicInstant::ORIGIN),
        Err(SleepTimeError::OutOfRange)
    );
    assert_eq!(timekeeper, before);
}

#[def_test]
fn sleep_injection_rejects_accumulation_overflow_without_partial_update() {
    let mut timekeeper = Timekeeper {
        sleep_time: TimeSpan::MAX,
        ..Timekeeper::INITIAL
    };
    let before = timekeeper;
    assert_eq!(
        timekeeper.inject_sleeptime(TimeSpan::from_nanos(1), MonotonicInstant::ORIGIN),
        Err(SleepTimeError::OutOfRange)
    );
    assert_eq!(timekeeper, before);
}

#[def_test]
fn sleep_injection_rejects_boottime_overflow_without_partial_update() {
    let mut timekeeper = Timekeeper::INITIAL;
    let before = timekeeper;
    let monotonic = MonotonicInstant::from_span_since_origin(TimeSpan::MAX);
    assert_eq!(
        timekeeper.inject_sleeptime(TimeSpan::from_nanos(1), monotonic),
        Err(SleepTimeError::OutOfRange)
    );
    assert_eq!(timekeeper, before);
}

#[def_test]
fn sleep_injection_rejects_counter_regression() {
    let mut timekeeper = Timekeeper::INITIAL;
    let monotonic = MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(10));
    assert!(
        timekeeper
            .set_realtime_checked(SystemTime::from_unix_seconds(1_000), monotonic)
            .is_ok()
    );
    let before = timekeeper;
    assert_eq!(
        timekeeper.inject_sleeptime(TimeSpan::from_secs(1), MonotonicInstant::ORIGIN),
        Err(SleepTimeError::InvalidMonotonicBaseline)
    );
    assert_eq!(timekeeper, before);
}

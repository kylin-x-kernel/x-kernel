// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use ktime_types::{NANOS_PER_SEC, TimeSpan};
use unittest::def_test;

#[def_test]
fn test_duration_from_nanos() {
    let nanos = NANOS_PER_SEC;
    let from = TimeSpan::from_nanos(nanos);
    let one = TimeSpan::from_secs(1);
    assert_eq!(from, one);
}

#[def_test]
fn test_duration_ordering() {
    let short = TimeSpan::from_millis(1);
    let long = TimeSpan::from_millis(2);
    assert!(long > short);
}


#[def_test]
fn timer_deadline_uses_rebased_logical_time() {
    use super::{epoch::ClockEpoch, timer_interval};
    use ktime_types::MonotonicInstant;

    let clock = ClockEpoch::new();
    let token = clock.suspend(|| 100).unwrap();
    clock.resume(token, || 10_000).unwrap();
    let (ticks, is_suspended) = clock.read(|| 10_020);
    // The hardware counter is 10_020, but the logical deadline is still 150.
    let now = MonotonicInstant::from_span_since_origin(TimeSpan::from_nanos(ticks));
    let deadline = MonotonicInstant::from_span_since_origin(TimeSpan::from_nanos(150));
    assert_eq!(timer_interval(deadline, now, is_suspended), Some(TimeSpan::from_nanos(30)));
    assert_eq!(timer_interval(now, deadline, is_suspended), Some(TimeSpan::ZERO));
}

#[def_test]
fn frozen_clock_suppresses_local_event_programming() {
    use super::timer_interval;
    use ktime_types::MonotonicInstant;

    let now = MonotonicInstant::from_span_since_origin(TimeSpan::from_nanos(100));
    let deadline = now + TimeSpan::from_nanos(50);
    assert_eq!(timer_interval(deadline, now, true), None);
}

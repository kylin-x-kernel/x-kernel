// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

use super::*;

#[def_test]
fn test_clock_ticks_round_trip_exact_values() {
    let span = TimeSpan::from_nanos(NANOS_PER_SEC / CLOCK_TICKS_PER_SECOND * 2);
    let ticks = PosixClockTicks::from_time_span(span);
    assert_eq!(ticks.as_raw(), 2);
    assert_eq!(ticks.to_time_span(), span);
}

#[def_test]
fn test_clock_ticks_truncate_sub_tick_duration() {
    assert_eq!(
        PosixClockTicks::from_time_span(TimeSpan::from_nanos(1)).as_raw(),
        0
    );
}

#[def_test]
fn test_clock_ticks_saturate_large_duration() {
    assert_eq!(
        PosixClockTicks::from_time_span(TimeSpan::MAX).as_raw(),
        u64::MAX
    );
}

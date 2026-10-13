// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use crate::*;

#[def_test]
fn frequency_conversions_preserve_hertz_and_make_rounding_explicit() {
    let frequency = Frequency::from_hz(2_400_000_999);

    assert_eq!(frequency.as_hz(), 2_400_000_999);
    assert_eq!(frequency.as_khz_floor(), 2_400_000);
    assert_eq!(frequency.as_mhz_floor(), 2_400);
    assert_eq!(
        Frequency::checked_from_khz(2_400_000),
        Some(Frequency::from_hz(2_400_000_000))
    );
    assert_eq!(
        Frequency::checked_from_mhz(2_400),
        Some(Frequency::from_hz(2_400_000_000))
    );
}

#[def_test]
fn frequency_scaled_constructors_reject_overflow() {
    assert_eq!(Frequency::checked_from_khz(u64::MAX), None);
    assert_eq!(Frequency::checked_from_mhz(u64::MAX), None);
}

#[def_test]
fn clock_domains_are_distinct() {
    let monotonic = MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(2));
    let boot = BoottimeInstant::from_span_since_origin(TimeSpan::from_secs(2));
    assert_eq!(monotonic.span_since_origin(), boot.span_since_origin());
}

#[def_test]
fn instant_arithmetic_preserves_domain() {
    let start = MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(3));
    let end = start + TimeSpan::from_millis(500);
    assert_eq!(end - start, TimeSpan::from_millis(500));
}

#[def_test]
fn nanosecond_boundary_conversion_saturates() {
    let representable = TimeSpan::new(2, 3);
    assert_eq!(representable.as_nanos_u64_saturating(), 2_000_000_003);

    let largest_boundary_value = TimeSpan::try_from_nanos(u64::MAX as u128).unwrap();
    assert_eq!(largest_boundary_value.as_nanos_u64_saturating(), u64::MAX);
    assert_eq!(TimeSpan::MAX.as_nanos_u64_saturating(), u64::MAX);

    let instant = MonotonicInstant::from_span_since_origin(TimeSpan::MAX);
    assert_eq!(instant.as_nanos_u64_saturating(), u64::MAX);
}

#[def_test]
fn system_time_supports_pre_epoch_values() {
    let timestamp = SystemTime::from_unix_nanos(-1).unwrap();
    assert_eq!(timestamp.unix_seconds(), -1);
    assert_eq!(timestamp.subsec_nanos(), 999_999_999);
    assert_eq!(timestamp.unix_nanos(), -1);
}

#[def_test]
fn system_time_reports_reversed_duration() {
    let before = SystemTime::from_unix_seconds(-2);
    let after = SystemTime::from_unix_seconds(3);
    assert_eq!(after.duration_since(before), Ok(TimeSpan::from_secs(5)));
    assert_eq!(
        before.duration_since(after).unwrap_err().duration(),
        TimeSpan::from_secs(5)
    );
}

#[def_test]
fn system_time_checked_arithmetic_normalizes_components() {
    let timestamp = SystemTime::from_unix_parts(10, 900_000_000).unwrap();
    assert_eq!(
        timestamp.checked_add(TimeSpan::from_millis(200)),
        SystemTime::from_unix_parts(11, 100_000_000)
    );

    let timestamp = SystemTime::from_unix_parts(10, 100_000_000).unwrap();
    assert_eq!(
        timestamp.checked_sub(TimeSpan::from_millis(200)),
        SystemTime::from_unix_parts(9, 900_000_000)
    );
}

#[def_test]
fn system_time_checked_arithmetic_detects_bounds() {
    assert_eq!(SystemTime::MAX.checked_add(TimeSpan::from_nanos(1)), None);

    assert_eq!(SystemTime::MIN.checked_sub(TimeSpan::from_nanos(1)), None);

    assert_eq!(
        SystemTime::MIN.checked_add(TimeSpan::from_secs(u64::MAX)),
        Some(SystemTime::from_unix_seconds(i64::MAX))
    );
}

#[def_test]
fn timestamp_limits_round_down_and_clamp() {
    let limits = TimestampLimits::new(NANOS_PER_SEC as u32, -10, 10);

    assert_eq!(
        limits.truncate(SystemTime::from_unix_parts(2, 123_456_789).unwrap()),
        SystemTime::from_unix_seconds(2)
    );
    assert_eq!(
        limits.truncate(SystemTime::from_unix_parts(-11, 999_999_999).unwrap()),
        SystemTime::from_unix_seconds(-10)
    );
    assert_eq!(
        limits.truncate(SystemTime::from_unix_parts(10, 123_456_789).unwrap()),
        SystemTime::from_unix_seconds(10)
    );
    assert_eq!(
        TimestampLimits::NANOSECOND.truncate(SystemTime::from_unix_parts(2, 123_456_789).unwrap()),
        SystemTime::from_unix_parts(2, 123_456_789).unwrap()
    );
    assert_eq!(TimestampLimits::default(), TimestampLimits::SECOND);
}

#[def_test]
fn timestamp_limits_clear_nanoseconds_at_linux_range_endpoints() {
    let limits = TimestampLimits::new(1, -10, 10);

    assert_eq!(
        limits.truncate(SystemTime::from_unix_parts(-10, 123_456_789).unwrap()),
        SystemTime::from_unix_seconds(-10)
    );
    assert_eq!(
        limits.truncate(SystemTime::from_unix_parts(10, 987_654_321).unwrap()),
        SystemTime::from_unix_seconds(10)
    );
    assert_eq!(
        limits.truncate(SystemTime::from_unix_parts(9, 987_654_321).unwrap()),
        SystemTime::from_unix_parts(9, 987_654_321).unwrap()
    );
}

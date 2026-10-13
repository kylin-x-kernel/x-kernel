// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

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

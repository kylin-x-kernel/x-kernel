// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

fn at(seconds: u64) -> MonotonicInstant {
    MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(seconds))
}

/// Warn after the threshold, throttle repeats, and reset on real progress.
/// # Panics
/// Panics on early, duplicate, missing or stale warnings.
#[def_test]
fn threshold_repetition_and_progress() {
    let record = StallRecord::new("queue".into(), 0.into());
    let threshold = TimeSpan::from_secs(10);
    assert!(record.take_warning(at(20), threshold).is_none());
    record.publish_interval(Some(at(10)));
    assert!(record.take_warning(at(19), threshold).is_none());
    assert_eq!(record.take_warning(at(20), threshold), Some(threshold));
    assert!(record.take_warning(at(29), threshold).is_none());
    assert_eq!(
        record.take_warning(at(30), threshold),
        Some(TimeSpan::from_secs(20))
    );
    record.publish_interval(Some(at(30)));
    assert!(record.take_warning(at(39), threshold).is_none());
    assert_eq!(record.take_warning(at(40), threshold), Some(threshold));
    record.publish_interval(None);
    assert!(record.take_warning(at(90), threshold).is_none());
}

/// A held publication lock is skipped; no spin or nested blocking acquisition.
/// # Panics
/// Panics if inspection waits for the held lock or changes warning history.
#[def_test]
fn busy_record_skips_without_consuming_warning() {
    let record = StallRecord::new("queue".into(), 0.into());
    record.publish_interval(Some(at(10)));
    let guard = record.state.lock();
    assert!(
        record
            .take_warning(at(30), TimeSpan::from_secs(10))
            .is_none()
    );
    drop(guard);
    assert!(
        record
            .take_warning(at(30), TimeSpan::from_secs(10))
            .is_some()
    );
}

/// Invalid observations do not consume an opportunity; full-range time is retained.
/// # Panics
/// Panics on overflow, reversed-time warning or lost history.
#[def_test]
fn invalid_time_and_full_range() {
    let record = StallRecord::new("queue".into(), 0.into());
    record.publish_interval(Some(at(10)));
    assert!(record.take_warning(at(9), TimeSpan::from_secs(1)).is_none());
    assert!(record.take_warning(at(20), TimeSpan::ZERO).is_none());
    assert!(
        record
            .take_warning(at(20), TimeSpan::from_secs(10))
            .is_some()
    );
    assert!(
        record
            .take_warning(at(19), TimeSpan::from_secs(1))
            .is_none()
    );
    record.publish_interval(Some(MonotonicInstant::ORIGIN));
    let max = MonotonicInstant::from_span_since_origin(TimeSpan::MAX);
    assert_eq!(record.take_warning(max, TimeSpan::MAX), Some(TimeSpan::MAX));
}

/// A new episode at the same timestamp does not inherit old report suppression.
/// # Panics
/// Panics if an obsolete warning history suppresses a newly published episode.
#[def_test]
fn same_timestamp_needs_no_version_counter() {
    let record = StallRecord::new("queue".into(), 0.into());
    record.publish_interval(Some(at(10)));
    assert!(
        record
            .take_warning(at(20), TimeSpan::from_secs(10))
            .is_some()
    );
    record.publish_interval(Some(at(10)));
    assert!(
        record
            .take_warning(at(20), TimeSpan::from_secs(10))
            .is_some()
    );
}

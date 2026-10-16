// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

#[def_test]
fn test_timespec_from_time_span() {
    let tv = TimeSpan::new(42, 123_456_789);
    let ts = timespec::from_time_span(tv);
    assert_eq!(ts.tv_sec, 42);
    assert_eq!(ts.tv_nsec, 123_456_789);
}

#[def_test]
fn test_timespec_try_into_time_span() {
    let ts = timespec {
        tv_sec: 10,
        tv_nsec: 500_000_000,
    };
    let tv = ts.try_into_time_span().unwrap();
    assert_eq!(tv.as_secs(), 10);
    assert_eq!(tv.subsec_nanos(), 500_000_000);
}

#[def_test]
fn test_timespec_invalid_nsec_negative() {
    let ts = timespec {
        tv_sec: 0,
        tv_nsec: -1,
    };
    assert!(ts.try_into_time_span().is_err());
}

#[def_test]
fn test_timespec_invalid_nsec_overflow() {
    let ts = timespec {
        tv_sec: 0,
        tv_nsec: 1_000_000_000,
    };
    assert!(ts.try_into_time_span().is_err());
}

#[def_test]
fn test_timespec_invalid_sec_negative() {
    let ts = timespec {
        tv_sec: -1,
        tv_nsec: 0,
    };
    assert!(ts.try_into_time_span().is_err());
}

#[def_test]
fn test_timeval_from_time_span() {
    let tv = TimeSpan::new(5, 123_456_000);
    let tval = timeval::from_time_span(tv);
    assert_eq!(tval.tv_sec, 5);
    assert_eq!(tval.tv_usec, 123_456);
}

#[def_test]
fn test_timeval_try_into_time_span() {
    let tval = timeval {
        tv_sec: 7,
        tv_usec: 999_999,
    };
    let tv = tval.try_into_time_span().unwrap();
    assert_eq!(tv.as_secs(), 7);
    assert_eq!(tv.subsec_nanos(), 999_999_000);
}

#[def_test]
fn test_timeval_invalid_usec_overflow() {
    let tval = timeval {
        tv_sec: 0,
        tv_usec: 1_000_000,
    };
    assert!(tval.try_into_time_span().is_err());
}

#[def_test]
fn test_timeval_invalid_usec_negative() {
    let tval = timeval {
        tv_sec: 0,
        tv_usec: -1,
    };
    assert!(tval.try_into_time_span().is_err());
}

#[def_test]
fn test_timespec_zero() {
    let ts = timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let tv = ts.try_into_time_span().unwrap();
    assert_eq!(tv.as_secs(), 0);
    assert_eq!(tv.subsec_nanos(), 0);
}

#[def_test]
fn test_timespec_boundary_nsec() {
    let ts = timespec {
        tv_sec: 0,
        tv_nsec: 999_999_999,
    };
    let tv = ts.try_into_time_span().unwrap();
    assert_eq!(tv.subsec_nanos(), 999_999_999);
}

#[def_test]
fn test_time_span_identity() {
    let tv = TimeSpan::new(100, 200);
    let tv2 = TimeSpan::from_time_span(tv);
    assert_eq!(tv.as_secs(), tv2.as_secs());
    assert_eq!(tv.subsec_nanos(), tv2.subsec_nanos());
}

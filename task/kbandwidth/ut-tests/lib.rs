// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use crate::Bandwidth;
use unittest::{assert, assert_eq, def_test};

/// Anchor for relative timestamps: the guest monotonic clock does not start
/// at zero, so tests initialize the tracker once and read back its anchor.
fn now() -> u64 {
    // Any nonzero base works: tests derive every timestamp from the anchor
    // readback, so the absolute value is irrelevant.
    1
}

fn initialized_bandwidth() -> Bandwidth {
    let bandwidth = Bandwidth::new();
    bandwidth.charge(0, now());
    bandwidth
}

#[def_test]
fn unlimited_bandwidth_never_throttles() {
    let bandwidth = Bandwidth::new();
    assert!(bandwidth.quota_us().is_none());
    let mut now_ns = now();
    for _ in 0..5 {
        bandwidth.charge(200_000, now_ns);
        now_ns += 50_000_000;
        bandwidth.poll(now_ns);
        assert!(!bandwidth.is_throttled());
        now_ns += 50_000_000;
    }
}

#[def_test]
fn quota_crossing_starts_one_throttle_window() {
    let bandwidth = initialized_bandwidth();
    bandwidth.set(Some(10_000), 100_000);
    let base_ns = bandwidth.period_anchor_ns();
    // Charge below quota: no throttle.
    assert!(!bandwidth.charge(5_000, base_ns));
    assert!(!bandwidth.is_throttled());
    // Crossing the quota starts exactly one window; repeat charges keep it.
    assert!(bandwidth.charge(10_000, base_ns + 1_000_000));
    assert!(bandwidth.is_throttled());
    assert!(!bandwidth.charge(10_000, base_ns + 2_000_000));
    assert_eq!(bandwidth.counters().1, 1);
    // Runtime inside the period accumulates.
    assert_eq!(bandwidth.runtime_us(), 25_000);
    // Polling inside the same period does not close the window.
    assert!(!bandwidth.poll(base_ns + 3_000_000));
    assert!(bandwidth.is_throttled());
    // The next period closes it and counts the throttled duration.
    assert!(bandwidth.poll(base_ns + 101_000_000));
    assert!(!bandwidth.is_throttled());
    let (nr_periods, nr_throttled, throttled_usec) = bandwidth.counters();
    assert!(nr_periods >= 1);
    assert_eq!(nr_throttled, 1);
    assert_eq!(throttled_usec, 100_000);
    // A fresh period charges from zero.
    assert_eq!(bandwidth.runtime_us(), 0);
}

#[def_test]
fn set_rejects_out_of_domain_values() {
    let bandwidth = Bandwidth::new();
    assert!(!bandwidth.set(Some(999), 100_000));
    assert!(!bandwidth.set(Some(10_000), 999));
    assert!(!bandwidth.set(Some(10_000), 1_000_001));
    assert!(bandwidth.set(Some(10_000), 100_000));
    assert!(bandwidth.set(None, 50_000));
    assert!(bandwidth.quota_us().is_none());
    assert_eq!(bandwidth.period_us(), 50_000);
}

#[def_test]
fn multi_period_gap_retires_elapsed_periods() {
    let bandwidth = initialized_bandwidth();
    bandwidth.set(Some(5_000), 100_000);
    let base_ns = bandwidth.period_anchor_ns();
    assert!(bandwidth.charge(10_000, base_ns));
    // Jump three periods ahead: the window closes and periods retire.
    assert!(bandwidth.poll(base_ns + 3 * 100_000_000));
    let (nr_periods, _, _) = bandwidth.counters();
    assert_eq!(nr_periods, 3);
}

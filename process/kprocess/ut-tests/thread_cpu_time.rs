// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::{PrevCputime, ThreadCpuTime};

#[def_test]
fn test_tick_counters_split_by_interrupted_context() {
    let time = ThreadCpuTime::new();
    assert_eq!(time.raw(), (0, 0));

    time.account_tick(true, 1000);
    assert_eq!(time.raw(), (1000, 0));

    time.account_tick(false, 1000);
    time.account_tick(false, 1000);
    assert_eq!(time.raw(), (1000, 2000));
}

#[def_test]
fn test_raw_reads_never_settle() {
    let time = ThreadCpuTime::new();
    time.account_tick(true, 40);
    time.account_tick(false, 60);

    // Repeated remote reads observe the committed counters unchanged; only the
    // owning CPU's next tick moves them.
    assert_eq!(time.raw(), (40, 60));
    assert_eq!(time.raw(), (40, 60));
    time.account_tick(true, 40);
    assert_eq!(time.raw(), (80, 60));
}

#[def_test]
fn test_adjust_scales_split_to_runtime() {
    let prev = PrevCputime::new();
    // stime = 10 * 90 / (10 + 20) = 30, utime = 90 - 30 = 60.
    assert_eq!(prev.adjust(20, 10, 90), (60, 30));

    // With no ticks in one mode, all runtime is charged to the other.
    let prev = PrevCputime::new();
    assert_eq!(prev.adjust(30, 0, 100), (100, 0));
    let prev = PrevCputime::new();
    assert_eq!(prev.adjust(0, 30, 100), (0, 100));

    // No runtime and no ticks: the split is zero.
    let prev = PrevCputime::new();
    assert_eq!(prev.adjust(0, 0, 0), (0, 0));
}

#[def_test]
fn test_adjust_monotonic_under_ratio_shift() {
    let prev = PrevCputime::new();
    assert_eq!(prev.adjust(80, 20, 100), (80, 20));

    // Kernel-heavy shift: both counters advance with the runtime.
    assert_eq!(prev.adjust(80, 120, 200), (80, 120));

    // User-heavy shift: stime clamps to its previous value, utime absorbs the
    // rest of the runtime.
    assert_eq!(prev.adjust(190, 10, 210), (90, 120));

    // Back to a balanced split at a larger runtime.
    assert_eq!(prev.adjust(280, 120, 400), (280, 120));

    // stime scales up but utime would regress: utime clamps and stime is
    // re-derived so the sum still equals the runtime.
    assert_eq!(prev.adjust(280, 200, 410), (280, 130));

    // A non-advancing or reordered runtime keeps the last published sample.
    assert_eq!(prev.adjust(280, 200, 410), (280, 130));
    assert_eq!(prev.adjust(280, 200, 400), (280, 130));

    // A constant ratio scales linearly with the runtime.
    let prev = PrevCputime::new();
    assert_eq!(prev.adjust(10, 10, 100), (50, 50));
    assert_eq!(prev.adjust(10, 10, 200), (100, 100));
}

#[def_test]
fn test_adjust_sum_equals_runtime_without_overflow() {
    let prev = PrevCputime::new();
    let half = u64::MAX / 2;
    let (utime, stime) = prev.adjust(half, half, u64::MAX);
    assert_eq!(utime.saturating_add(stime), u64::MAX);
    assert!(utime > 0 && stime > 0);
}
// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use ktime_types::{MonotonicInstant, TimeSpan};
use unittest::def_test;

use super::{CpuTimeState, CpuTimeStatistics};

fn at(ns: u64) -> MonotonicInstant {
    MonotonicInstant::from_span_since_origin(TimeSpan::from_nanos(ns))
}

fn sample_ns(time: &mut CpuTimeStatistics, ns: u64) -> (u64, u64) {
    let (user, kernel) = time.sample(at(ns));
    (
        user.as_nanos_u64_saturating(),
        kernel.as_nanos_u64_saturating(),
    )
}

#[def_test]
fn test_cpu_time_inactive_intervals_are_not_charged() {
    let mut time = CpuTimeStatistics::new();
    assert_eq!(sample_ns(&mut time, 100), (0, 0));
    time.set_state(CpuTimeState::Kernel, at(100));
    time.set_state(CpuTimeState::Inactive, at(120));
    assert_eq!(sample_ns(&mut time, 1000), (0, 20));
    time.set_state(CpuTimeState::Kernel, at(2000));
    assert_eq!(sample_ns(&mut time, 2010), (0, 30));
}

#[def_test]
fn test_cpu_time_samples_do_not_double_charge() {
    let mut time = CpuTimeStatistics::new();
    time.set_state(CpuTimeState::Kernel, at(10));
    time.set_state(CpuTimeState::User, at(30));
    assert_eq!(sample_ns(&mut time, 40), (10, 20));
    assert_eq!(sample_ns(&mut time, 40), (10, 20));
    assert_eq!(time.utime, TimeSpan::from_nanos(10));
    assert_eq!(time.last_wall, Some(at(40)));
    time.set_state(CpuTimeState::Kernel, at(60));
    assert_eq!(sample_ns(&mut time, 70), (30, 30));
    time.set_state(CpuTimeState::Inactive, at(80));
    assert_eq!(sample_ns(&mut time, 10000), (30, 40));
}

#[def_test]
fn test_cpu_time_same_state_settles_once() {
    let mut time = CpuTimeStatistics::new();
    time.set_state(CpuTimeState::Kernel, at(10));
    time.set_state(CpuTimeState::Kernel, at(20));
    assert_eq!(sample_ns(&mut time, 30), (0, 20));
    assert_eq!(sample_ns(&mut time, 30), (0, 20));
}

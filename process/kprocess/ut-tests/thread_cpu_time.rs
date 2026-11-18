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

    time.activate(at(100));
    time.deactivate(at(120));
    assert_eq!(sample_ns(&mut time, 1000), (0, 20));

    time.activate(at(2000));
    assert_eq!(sample_ns(&mut time, 2010), (0, 30));
}

#[def_test]
fn test_cpu_time_samples_do_not_double_charge() {
    let mut time = CpuTimeStatistics::new();
    time.activate(at(10));
    time.switch_mode(CpuTimeState::User, at(10));

    assert_eq!(sample_ns(&mut time, 40), (30, 0));
    assert_eq!(sample_ns(&mut time, 40), (30, 0));
}

#[def_test]
fn test_cpu_time_final_publication_drops_later_intervals() {
    let mut time = CpuTimeStatistics::new();
    time.activate(at(0));
    time.finalize(at(50));
    assert_eq!(sample_ns(&mut time, 50), (0, 50));

    // A switch-in after publication must not reopen the interval.
    time.activate(at(80));
    assert_eq!(sample_ns(&mut time, 100), (0, 50));
}

#[def_test]
fn test_cpu_time_preserves_user_intervals_without_timer_ticks() {
    let mut time = CpuTimeStatistics::new();
    time.activate(at(10));
    time.switch_mode(CpuTimeState::User, at(20));
    time.switch_mode(CpuTimeState::Kernel, at(40));
    time.switch_mode(CpuTimeState::User, at(50));
    time.switch_mode(CpuTimeState::Kernel, at(80));
    time.finalize(at(90));

    // A child may reach exit without any sample between its user-mode runs.
    assert_eq!(sample_ns(&mut time, 100), (50, 30));
}

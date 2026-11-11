// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use ktime_types::{MonotonicInstant, TimeSpan};
use unittest::def_test;

use super::{CpuTimeState, CpuTimeStatistics};

fn at(ns: u64) -> MonotonicInstant {
    MonotonicInstant::from_span_since_origin(TimeSpan::from_nanos(ns))
}

fn sample_ns(time: &CpuTimeStatistics, ns: u64) -> (u64, u64) {
    let (user, kernel) = time.sample(at(ns));
    (
        user.as_nanos_u64_saturating(),
        kernel.as_nanos_u64_saturating(),
    )
}

#[def_test]
fn test_cpu_time_inactive_intervals_are_not_charged() {
    let time = CpuTimeStatistics::new();
    assert_eq!(sample_ns(&time, 100), (0, 0));

    time.activate(at(100));
    time.deactivate(at(120));
    assert_eq!(sample_ns(&time, 1000), (0, 20));

    time.activate(at(2000));
    assert_eq!(sample_ns(&time, 2010), (0, 30));
}

#[def_test]
fn test_cpu_time_samples_do_not_double_charge() {
    let time = CpuTimeStatistics::new();
    time.activate(at(10));
    time.record_mode(CpuTimeState::User);

    assert_eq!(sample_ns(&time, 40), (30, 0));
    assert_eq!(sample_ns(&time, 40), (30, 0));
    assert_eq!(time.output(), (TimeSpan::from_nanos(30), TimeSpan::ZERO));
}

#[def_test]
fn test_cpu_time_ticks_split_user_and_kernel_time() {
    let time = CpuTimeStatistics::new();
    time.activate(at(0));
    time.record_mode(CpuTimeState::User);
    time.settle(at(20));
    time.record_mode(CpuTimeState::Kernel);
    time.settle(at(30));

    assert_eq!(
        time.output(),
        (TimeSpan::from_nanos(20), TimeSpan::from_nanos(10))
    );
}

#[def_test]
fn test_cpu_time_boundary_only_publishes_mode() {
    let time = CpuTimeStatistics::new();
    time.activate(at(10));
    // Rapid user/kernel flips without settlement do not charge anything.
    time.record_mode(CpuTimeState::User);
    time.record_mode(CpuTimeState::Kernel);
    time.record_mode(CpuTimeState::User);
    assert_eq!(time.output(), (TimeSpan::ZERO, TimeSpan::ZERO));

    // The next settlement charges the whole interval to the current mode.
    time.settle(at(40));
    assert_eq!(
        time.output(),
        (TimeSpan::from_nanos(30), TimeSpan::ZERO)
    );
}

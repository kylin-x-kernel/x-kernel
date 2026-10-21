// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use ksignal::Signo;
use ktime_types::{ProcessCpuInstant, TimeSpan};
use posix_types::ITimerType;
use unittest::def_test;

use super::{ITimer, TimerInstant, timer_signal};

fn cpu_instant(nanos: u64) -> TimerInstant {
    TimerInstant::ProcessCpu(ProcessCpuInstant::from_span_since_origin(
        TimeSpan::from_nanos(nanos),
    ))
}

#[def_test]
fn test_timer_signal_mapping() {
    assert_eq!(timer_signal(ITimerType::Real), Signo::SIGALRM);
    assert_eq!(timer_signal(ITimerType::Virtual), Signo::SIGVTALRM);
    assert_eq!(timer_signal(ITimerType::Prof), Signo::SIGPROF);
}

#[def_test]
fn test_time_value_from_nanos_basic() {
    let tv = TimeSpan::from_nanos(0);
    assert_eq!(tv.as_secs(), 0);
    assert_eq!(tv.subsec_nanos(), 0);
}

#[def_test]
fn test_time_value_from_nanos_subsec() {
    let tv = TimeSpan::from_nanos(500_000_000);
    assert_eq!(tv.as_secs(), 0);
    assert_eq!(tv.subsec_nanos(), 500_000_000);
}

#[def_test]
fn test_time_value_from_nanos_multi_sec() {
    let tv = TimeSpan::from_nanos(2_500_000_000);
    assert_eq!(tv.as_secs(), 2);
    assert_eq!(tv.subsec_nanos(), 500_000_000);
}

#[def_test]
fn test_itimer_update_zero_remained() {
    let mut timer = ITimer::default();
    assert_eq!(timer.update(cpu_instant(100)), 0);
}

#[def_test]
fn test_itimer_update_counts_down_without_firing() {
    let mut timer = ITimer {
        interval: TimeSpan::ZERO,
        deadline: Some(cpu_instant(10)),
        runtime_deadline: None,
        alarm_pid: None,
        alarm_ticket: None,
    };
    assert_eq!(timer.update(cpu_instant(3)), 0);
    assert_eq!(timer.remaining(cpu_instant(3)), TimeSpan::from_nanos(7));
}

#[def_test]
fn test_itimer_update_fires_and_resets_to_interval() {
    let mut timer = ITimer {
        interval: TimeSpan::ZERO,
        deadline: Some(cpu_instant(5)),
        runtime_deadline: None,
        alarm_pid: None,
        alarm_ticket: None,
    };
    assert_eq!(timer.update(cpu_instant(5)), 1);
    assert_eq!(timer.remaining(cpu_instant(5)), TimeSpan::ZERO);
}

#[def_test]
fn test_itimer_update_rearms_from_previous_deadline() {
    let mut timer = ITimer {
        interval: TimeSpan::from_nanos(10),
        deadline: Some(cpu_instant(10)),
        runtime_deadline: None,
        alarm_pid: None,
        alarm_ticket: None,
    };

    assert_eq!(timer.update(cpu_instant(35)), 3);
    assert_eq!(timer.remaining(cpu_instant(35)), TimeSpan::from_nanos(5));
}

#[def_test]
fn test_itimer_update_periodic_rearms_from_previous_deadline() {
    let mut timer = ITimer {
        interval: TimeSpan::from_nanos(5),
        deadline: Some(cpu_instant(3)),
        runtime_deadline: None,
        alarm_pid: None,
        alarm_ticket: None,
    };

    assert_eq!(timer.update(cpu_instant(8)), 2);
    assert_eq!(timer.remaining(cpu_instant(8)), TimeSpan::from_nanos(5));
}

#[def_test]
fn consumed_alarm_rearms_after_clock_recheck_without_duplicate_entry() {
    use alloc::sync::Arc;
    use ktime_types::SystemTime;
    let now = ktime::realtime();
    let deadline = now.checked_add(TimeSpan::from_secs(60)).unwrap();
    let mut timer = ITimer::default();
    timer.set(TimerInstant::Realtime(now), TimeSpan::ZERO, Some(TimerInstant::Realtime(deadline)));
    timer.set_alarm(Some(TimerInstant::Realtime(deadline)), Some(1));
    let first = timer.alarm_ticket.as_ref().unwrap().clone();
    first.consume();
    // Runtime already removed the entry, but owner recheck is before deadline.
    assert_eq!(timer.update(TimerInstant::Realtime(SystemTime::from_unix_seconds(0))), 0);
    timer.set_alarm(Some(TimerInstant::Realtime(deadline)), Some(1));
    let replacement = timer.alarm_ticket.as_ref().unwrap().clone();
    assert!(!Arc::ptr_eq(&first, &replacement));
    assert!(replacement.is_queued());
    timer.set_alarm(Some(TimerInstant::Realtime(deadline)), Some(1));
    assert!(Arc::ptr_eq(&replacement, timer.alarm_ticket.as_ref().unwrap()));
}

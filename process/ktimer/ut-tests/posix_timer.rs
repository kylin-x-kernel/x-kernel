// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

#[def_test]
fn relative_realtime_overflow_preserves_timer_state() {
    let mut timer = PosixTimer::new(
        PosixTimerClock::Realtime,
        PosixTimerCreateNotify::None,
        1,
        1,
    );
    timer
        .settime(
            false,
            TimeSpan::from_secs(2),
            TimeSpan::from_secs(10),
            TimeSpan::ZERO,
            TimeSpan::ZERO,
        )
        .unwrap();
    let deadline = timer.spec.deadline();

    assert_eq!(
        timer.settime(
            false,
            TimeSpan::ZERO,
            TimeSpan::MAX,
            TimeSpan::ZERO,
            TimeSpan::ZERO,
        ),
        Err(KError::InvalidInput)
    );
    assert_eq!(timer.spec.deadline(), deadline);
    assert_eq!(
        timer
            .spec
            .snapshot(timer.armed_clock.now(TimeSpan::ZERO, TimeSpan::ZERO))
            .0,
        TimeSpan::from_secs(2)
    );
}

/// Overrun bookkeeping narrows and accumulates with saturation; the
/// `i32::MAX` clamp happens only at `timer_getoverrun()`. A truncating
/// narrowing drops exactly `2^32` expirations to zero and reports no
/// overrun at all.
#[def_test]
fn whole_counter_width_expirations_saturate_instead_of_wrapping() {
    let mut timer = PosixTimer::new(
        PosixTimerClock::ProcessCpu,
        PosixTimerCreateNotify::Signal {
            signo: Signo::SIGALRM,
            target_tid: None,
            value: PosixTimerSigValue::TimerId,
        },
        1,
        1,
    );
    timer
        .settime(
            false,
            TimeSpan::from_nanos(1),
            TimeSpan::from_nanos(1),
            TimeSpan::ZERO,
            TimeSpan::ZERO,
        )
        .unwrap();
    // The deadline is one nanosecond past the CPU-time origin, so a `2^32`
    // nanosecond total leaves `2^32 - 1` overdue nanoseconds and `2^32`
    // skipped periods at a one-nanosecond interval.
    let expirations = timer.update(TimeSpan::from_nanos(1u64 << 32), TimeSpan::ZERO);
    assert_eq!(expirations, 1usize << 32);

    let delivery = timer
        .collect_delivery(1, expirations)
        .expect("expirations produce a notification");
    let TimerDelivery::Process(TimerSignal::Posix { overrun, .. }) = delivery else {
        panic!("expected a process-directed POSIX timer signal");
    };
    assert_eq!(overrun, i32::MAX);
    timer.on_signal_dequeued();
    assert_eq!(timer.overrun(), i32::MAX);
}

#[def_test]
fn realtime_rearming_snapshots_the_previous_clock_domain() {
    let mut timer = PosixTimer::new(PosixTimerClock::Realtime, PosixTimerCreateNotify::None, 1, 1);
    timer.settime(false, TimeSpan::from_secs(2), TimeSpan::from_secs(60), TimeSpan::ZERO, TimeSpan::ZERO).unwrap();
    assert!(matches!(timer.spec.deadline(), Some(TimerInstant::Monotonic(_))));
    let absolute = ktime::realtime().duration_since(SystemTime::UNIX_EPOCH).unwrap().saturating_add(TimeSpan::from_secs(90));
    let old = timer.settime(true, TimeSpan::ZERO, absolute, TimeSpan::ZERO, TimeSpan::ZERO).unwrap();
    assert_eq!(old.0, TimeSpan::from_secs(2));
    assert!(old.1 <= TimeSpan::from_secs(60));
    assert!(matches!(timer.spec.deadline(), Some(TimerInstant::Realtime(_))));
    let old = timer.settime(false, TimeSpan::ZERO, TimeSpan::from_secs(60), TimeSpan::ZERO, TimeSpan::ZERO).unwrap();
    assert!(old.1 <= TimeSpan::from_secs(90));
    assert!(matches!(timer.spec.deadline(), Some(TimerInstant::Monotonic(_))));
}

#[def_test]
fn boottime_alarm_preserves_original_deadline() {
    let mut timer = PosixTimer::new(PosixTimerClock::Boottime, PosixTimerCreateNotify::None, 1, 1);
    timer.settime(false, TimeSpan::ZERO, TimeSpan::from_secs(60), TimeSpan::ZERO, TimeSpan::ZERO).unwrap();
    // Set alarm state without publishing a global queue entry.
    assert!(matches!(timer.spec.deadline(), Some(TimerInstant::Boottime(_))));
    assert!(timer.armed_clock == PosixTimerClock::Boottime);
}

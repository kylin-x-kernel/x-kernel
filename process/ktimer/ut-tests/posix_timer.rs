// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

struct InactiveOrigin;

impl TimerSignalOrigin for InactiveOrigin {
    fn complete(&self, _: i32, _: u64) -> ksignal::SignalDequeueAction {
        ksignal::SignalDequeueAction::Drop
    }
}

fn new_timer(
    clock: PosixTimerClock,
    notify: PosixTimerCreateNotify,
    timer_id: i32,
    signal_seq: u64,
) -> PosixTimer {
    PosixTimer::new(
        clock,
        notify,
        timer_id,
        signal_seq,
        Weak::<InactiveOrigin>::new(),
        TimerSignalTarget::process(Weak::new()),
        match notify {
            PosixTimerCreateNotify::None => None,
            _ => Some(PreparedTimerRecord::new(Weak::<InactiveOrigin>::new()).unwrap()),
        },
    )
}

#[def_test]
fn rearmed_one_shot_waits_for_old_record_without_overwriting_payload() {
    let mut timer = new_timer(
        PosixTimerClock::ProcessCpu,
        PosixTimerCreateNotify::Signal {
            signo: Signo::SIGRTMIN,
            target_tid: None,
            value: PosixTimerSigValue::TimerId,
        },
        1,
        1,
    );
    timer
        .settime(
            false,
            TimeSpan::ZERO,
            TimeSpan::from_nanos(1),
            TimeSpan::ZERO,
        )
        .unwrap();
    let expired = timer.update(TimeSpan::from_nanos(1));
    let TimerDelivery::Process(TimerSignal::Posix {
        notification: old, ..
    }) = timer.collect_delivery(1, expired).unwrap()
    else {
        panic!("expected POSIX notification");
    };
    let storage = timer.record.as_ref().unwrap().clone();
    timer
        .settime(
            false,
            TimeSpan::ZERO,
            TimeSpan::from_nanos(1),
            TimeSpan::from_nanos(1),
        )
        .unwrap();
    timer.set_signal_seq(2);
    let expired = timer.update(TimeSpan::from_nanos(2));
    assert!(timer.collect_delivery(1, expired).is_none());
    assert!(!timer.is_armed());
    assert_eq!(timer.deferred_expirations, 1);
    assert_eq!(old.receipt().unwrap().signal_seq(), 1);
    assert_eq!(old.info().timer_overrun(), Some(0));
    drop(old);
    let TimerDelivery::Process(TimerSignal::Posix { notification, .. }) =
        timer.collect_delivery(1, 0).unwrap()
    else {
        panic!("expected deferred POSIX notification");
    };
    assert_eq!(notification.receipt().unwrap().signal_seq(), 2);
    assert_eq!(notification.info().timer_overrun(), Some(0));
    assert!(timer.owns_record(&storage));
    assert_eq!(timer.deferred_expirations, 0);
    assert!(timer.collect_delivery(1, 0).is_none());
}

#[def_test]
fn accounting_cut_defers_new_expirations_until_receipt_release() {
    let mut timer = new_timer(
        PosixTimerClock::ProcessCpu,
        PosixTimerCreateNotify::Signal {
            signo: Signo::SIGRTMIN,
            target_tid: None,
            value: PosixTimerSigValue::TimerId,
        },
        1,
        1,
    );
    let old = timer.collect_delivery(1, 4).unwrap();
    timer.on_signal_dequeued();
    assert_eq!(timer.overrun(), 3);
    assert!(timer.collect_delivery(1, 3).is_none());
    assert_eq!(timer.deferred_expirations, 3);
    assert_eq!(timer.overrun(), 3);
    drop(old);
    let TimerDelivery::Process(TimerSignal::Posix { notification, .. }) =
        timer.collect_delivery(1, 0).unwrap()
    else {
        panic!("expected deferred POSIX notification");
    };
    assert_eq!(notification.info().timer_overrun(), Some(2));
    assert_eq!(timer.overrun(), 3);
    timer.on_signal_dequeued();
    assert_eq!(timer.overrun(), 2);
}

#[def_test]
fn reservation_failure_is_before_admission_and_sigev_none_skips_it() {
    let mut attempts = 0;
    let result = PosixTimerConfig::new_with_reservation(
        CLOCK_PROCESS_CPUTIME_ID as i32,
        PosixTimerCreateNotify::Signal {
            signo: Signo::SIGRTMIN,
            target_tid: None,
            value: PosixTimerSigValue::TimerId,
        },
        Weak::<InactiveOrigin>::new(),
        TimerSignalTarget::process(Weak::new()),
        |_| {
            attempts += 1;
            Err(KError::NoMemory)
        },
    );
    assert!(matches!(result, Err(KError::NoMemory)));
    assert_eq!(attempts, 1);
    let config = PosixTimerConfig::new_with_reservation(
        CLOCK_PROCESS_CPUTIME_ID as i32,
        PosixTimerCreateNotify::None,
        Weak::<InactiveOrigin>::new(),
        TimerSignalTarget::process(Weak::new()),
        |_| {
            attempts += 1;
            Err(KError::NoMemory)
        },
    )
    .unwrap();
    assert!(config.record.is_none());
    assert_eq!(attempts, 1);
    let result = PosixTimerConfig::new_with_reservation(
        -1,
        PosixTimerCreateNotify::None,
        Weak::<InactiveOrigin>::new(),
        TimerSignalTarget::process(Weak::new()),
        |_| {
            attempts += 1;
            Err(KError::NoMemory)
        },
    );
    assert!(matches!(result, Err(KError::InvalidInput)));
    assert_eq!(attempts, 1);
}

#[def_test]
fn relative_realtime_overflow_preserves_timer_state() {
    let mut timer = new_timer(
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
        )
        .unwrap();
    let deadline = timer.spec.deadline();

    assert_eq!(
        timer.settime(
            false,
            TimeSpan::ZERO,
            TimeSpan::MAX,
            TimeSpan::ZERO,
        ),
        Err(KError::InvalidInput)
    );
    assert_eq!(timer.spec.deadline(), deadline);
    assert_eq!(
        timer
            .spec
            .snapshot(timer.armed_clock.now(TimeSpan::ZERO))
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
    let mut timer = new_timer(
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
        )
        .unwrap();
    // The deadline is one nanosecond past the CPU-time origin, so a `2^32`
    // nanosecond total leaves `2^32 - 1` overdue nanoseconds and `2^32`
    // skipped periods at a one-nanosecond interval.
    let expirations = timer.update(TimeSpan::from_nanos(1u64 << 32));
    assert_eq!(expirations, 1usize << 32);

    let delivery = timer
        .collect_delivery(1, expirations)
        .expect("expirations produce a notification");
    let TimerDelivery::Process(TimerSignal::Posix { notification, .. }) = delivery else {
        panic!("expected a process-directed POSIX timer signal");
    };
    assert_eq!(notification.info().timer_overrun(), Some(i32::MAX));
    timer.on_signal_dequeued();
    assert_eq!(timer.overrun(), i32::MAX);
}

#[def_test]
fn realtime_rearming_snapshots_the_previous_clock_domain() {
    let mut timer = new_timer(
        PosixTimerClock::Realtime,
        PosixTimerCreateNotify::None,
        1,
        1,
    );
    timer
        .settime(
            false,
            TimeSpan::from_secs(2),
            TimeSpan::from_secs(60),
            TimeSpan::ZERO,
        )
        .unwrap();
    assert!(matches!(
        timer.spec.deadline(),
        Some(TimerInstant::Monotonic(_))
    ));
    let absolute = ktime::realtime()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .saturating_add(TimeSpan::from_secs(90));
    let old = timer
        .settime(
            true,
            TimeSpan::ZERO,
            absolute,
            TimeSpan::ZERO,
        )
        .unwrap();
    assert_eq!(old.0, TimeSpan::from_secs(2));
    assert!(old.1 <= TimeSpan::from_secs(60));
    assert!(matches!(
        timer.spec.deadline(),
        Some(TimerInstant::Realtime(_))
    ));
    let old = timer
        .settime(
            false,
            TimeSpan::ZERO,
            TimeSpan::from_secs(60),
            TimeSpan::ZERO,
        )
        .unwrap();
    assert!(old.1 <= TimeSpan::from_secs(90));
    assert!(matches!(
        timer.spec.deadline(),
        Some(TimerInstant::Monotonic(_))
    ));
}

#[def_test]
fn boottime_alarm_preserves_original_deadline() {
    let mut timer = new_timer(
        PosixTimerClock::Boottime,
        PosixTimerCreateNotify::None,
        1,
        1,
    );
    timer
        .settime(
            false,
            TimeSpan::ZERO,
            TimeSpan::from_secs(60),
            TimeSpan::ZERO,
        )
        .unwrap();
    // Set alarm state without publishing a global queue entry.
    assert!(matches!(
        timer.spec.deadline(),
        Some(TimerInstant::Boottime(_))
    ));
    assert!(timer.armed_clock == PosixTimerClock::Boottime);
}

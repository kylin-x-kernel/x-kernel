// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use khal::time::monotonic_time;
use ksignal::Signo;
use ktime_types::TimeSpan;
use linux_raw_sys::general::{CLOCK_MONOTONIC, CLOCK_PROCESS_CPUTIME_ID};
use posix_types::ITimerType;
use unittest::def_test;

use super::ProcessTimerManager;
use crate::{
    TimerDelivery, TimerSignal,
    posix_timer::{PosixTimerCreateNotify, PosixTimerSigValue},
};

#[def_test]
fn test_any_cpu_timer_armed_tracks_arm_and_disarm() {
    let mut manager = ProcessTimerManager::new(1);
    assert!(!manager.any_cpu_timer_armed());

    // Wall-clock itimers never arm the CPU-timer gate.
    manager.set_itimer(
        ITimerType::Real,
        TimeSpan::from_nanos(10),
        TimeSpan::from_nanos(10),
        TimeSpan::ZERO,
        TimeSpan::ZERO,
    );
    assert!(!manager.any_cpu_timer_armed());

    manager.set_itimer(
        ITimerType::Virtual,
        TimeSpan::ZERO,
        TimeSpan::from_nanos(10),
        TimeSpan::ZERO,
        TimeSpan::ZERO,
    );
    assert!(manager.any_cpu_timer_armed());

    // One-shot expiry disarms through update(); the gate mirrors that.
    manager.poll_cpu_timers(TimeSpan::from_nanos(10), TimeSpan::ZERO);
    assert!(!manager.any_cpu_timer_armed());

    // Process-CPU POSIX timers participate in the gate.
    let timer_id = manager
        .create_posix_timer(
            CLOCK_PROCESS_CPUTIME_ID as i32,
            PosixTimerCreateNotify::Signal {
                signo: Signo::SIGRTMIN,
                target_tid: None,
                value: PosixTimerSigValue::TimerId,
            },
        )
        .unwrap();
    manager
        .set_posix_timer(
            timer_id,
            false,
            TimeSpan::from_nanos(5),
            TimeSpan::from_nanos(5),
            TimeSpan::ZERO,
            TimeSpan::ZERO,
        )
        .unwrap();
    assert!(manager.any_cpu_timer_armed());
    manager.delete_posix_timer(timer_id).unwrap();
    assert!(!manager.any_cpu_timer_armed());
}

#[def_test]
fn test_process_timer_manager_set_itimer_returns_previous_values() {
    let mut manager = ProcessTimerManager::new(1);

    let (old_interval, old_remained) = manager.set_itimer(
        ITimerType::Virtual,
        TimeSpan::from_nanos(11),
        TimeSpan::from_nanos(22),
        TimeSpan::ZERO,
        TimeSpan::ZERO,
    );
    assert_eq!(old_interval.as_secs(), 0);
    assert_eq!(old_remained.as_secs(), 0);

    let (old_interval, old_remained) = manager.set_itimer(
        ITimerType::Virtual,
        TimeSpan::from_nanos(33),
        TimeSpan::from_nanos(44),
        TimeSpan::ZERO,
        TimeSpan::ZERO,
    );
    assert_eq!(old_interval.subsec_nanos(), 11);
    assert_eq!(old_remained.subsec_nanos(), 22);

    let (interval, remained) =
        manager.get_itimer(ITimerType::Virtual, TimeSpan::ZERO, TimeSpan::ZERO);
    assert_eq!(interval.subsec_nanos(), 33);
    assert_eq!(remained.subsec_nanos(), 44);
}

#[def_test]
fn test_posix_timer_signal_seq_invalidates_stale_signal() {
    let mut manager = ProcessTimerManager::new(1);
    let timer_id = manager
        .create_posix_timer(
            CLOCK_PROCESS_CPUTIME_ID as i32,
            PosixTimerCreateNotify::Signal {
                signo: Signo::SIGRTMIN,
                target_tid: None,
                value: PosixTimerSigValue::TimerId,
            },
        )
        .unwrap();
    let (_, delivery) = manager
        .set_posix_timer(
            timer_id,
            false,
            TimeSpan::ZERO,
            TimeSpan::from_nanos(1),
            TimeSpan::ZERO,
            TimeSpan::ZERO,
        )
        .unwrap();
    assert!(delivery.is_none());

    let delivery = manager
        .poll_cpu_timers(TimeSpan::from_nanos(1), TimeSpan::ZERO)
        .into_iter()
        .next()
        .expect("timer delivery");
    let stale_signal_seq = match delivery {
        TimerDelivery::Process(TimerSignal::Posix { signal_seq, .. }) => signal_seq,
        _ => panic!("expected a POSIX timer signal"),
    };

    let _ = manager
        .set_posix_timer(
            timer_id,
            false,
            TimeSpan::ZERO,
            TimeSpan::from_nanos(10),
            TimeSpan::from_nanos(1),
            TimeSpan::ZERO,
        )
        .unwrap();
    assert!(!manager.on_timer_signal_dequeued(
        timer_id,
        stale_signal_seq,
        TimeSpan::from_nanos(1),
        TimeSpan::ZERO,
    ));
}

/// POSIX delivers at most one notification per pending timer signal. An
/// interval that elapses between the last collection (the alarm task) and
/// the delivery belongs to that notification's overrun, and must not be
/// delivered as a second signal.
#[def_test]
fn dequeued_notification_absorbs_intervals_due_before_delivery() {
    let mut manager = ProcessTimerManager::new(1);
    let timer_id = manager
        .create_posix_timer(
            CLOCK_PROCESS_CPUTIME_ID as i32,
            PosixTimerCreateNotify::Signal {
                signo: Signo::SIGALRM,
                target_tid: None,
                value: PosixTimerSigValue::TimerId,
            },
        )
        .unwrap();
    let (_, delivery) = manager
        .set_posix_timer(
            timer_id,
            false,
            TimeSpan::from_nanos(3),
            TimeSpan::from_nanos(2),
            TimeSpan::ZERO,
            TimeSpan::ZERO,
        )
        .unwrap();
    assert!(delivery.is_none());
    // The first interval produces the notification; the two intervals that
    // elapse afterwards are still uncollected when it is dequeued.
    let delivery = manager
        .poll_cpu_timers(TimeSpan::from_nanos(2), TimeSpan::ZERO)
        .into_iter()
        .next()
        .expect("timer delivery");
    let signal_seq = match delivery {
        TimerDelivery::Process(TimerSignal::Posix { signal_seq, .. }) => signal_seq,
        _ => panic!("expected a POSIX timer signal"),
    };
    assert!(manager.on_timer_signal_dequeued(
        timer_id,
        signal_seq,
        TimeSpan::from_nanos(8),
        TimeSpan::ZERO,
    ));
    // Both elapsed intervals belong to the delivered notification.
    assert_eq!(manager.get_posix_timer_overrun(timer_id), Ok(2));
    // They were absorbed, so the timer stays quiet until the next interval.
    assert!(
        manager
            .poll_cpu_timers(TimeSpan::from_nanos(8), TimeSpan::ZERO)
            .is_empty()
    );
    assert_eq!(
        manager
            .poll_cpu_timers(TimeSpan::from_nanos(11), TimeSpan::ZERO)
            .len(),
        1
    );
}

/// The dequeue catch-up advances the interval schedule, so the alarm entry
/// must follow the new deadline. Without the re-arm the next alarm wakeup
/// finds no expiry to report and a periodic timer silently stops
/// notifying.
#[def_test]
fn dequeued_notification_rearms_the_advanced_alarm() {
    let mut manager = ProcessTimerManager::new(1);
    let timer_id = manager
        .create_posix_timer(
            CLOCK_MONOTONIC as i32,
            PosixTimerCreateNotify::Signal {
                signo: Signo::SIGRTMIN,
                target_tid: None,
                value: PosixTimerSigValue::TimerId,
            },
        )
        .unwrap();
    // Arm with an absolute deadline one second in the past: the arming
    // itself reports the expirations (whatever the clock's granularity is)
    // and re-arms the alarm for the advanced period.
    let past = monotonic_time()
        .span_since_origin()
        .saturating_sub(TimeSpan::from_secs(1));
    let (_, delivery) = manager
        .set_posix_timer(
            timer_id,
            true,
            TimeSpan::from_millis(1),
            past,
            TimeSpan::ZERO,
            TimeSpan::ZERO,
        )
        .unwrap();
    let TimerDelivery::Process(TimerSignal::Posix { signal_seq, .. }) =
        delivery.expect("a past deadline expires on arming")
    else {
        panic!("expected a POSIX timer signal");
    };
    let armed_after_arming = manager
        .posix_timer_alarm_deadline(timer_id)
        .expect("arming re-arms the next period");
    // The catch-up can only fold intervals the clock has already passed, so
    // wait for the armed deadline in the clock's own domain: the bound is
    // expressed on that same clock so a slow tick cannot end the wait
    // early. A clock that stops advancing fails the assertion below rather
    // than hanging the suite.
    let wait_limit = armed_after_arming + TimeSpan::from_secs(5);
    while armed_after_arming.saturating_duration_since(monotonic_time()) > TimeSpan::ZERO
        && wait_limit.saturating_duration_since(monotonic_time()) > TimeSpan::ZERO
    {
        ktask::sleep(TimeSpan::from_millis(1));
    }
    assert!(
        manager.on_timer_signal_dequeued(timer_id, signal_seq, TimeSpan::ZERO, TimeSpan::ZERO,)
    );
    let armed_after_dequeue = manager
        .posix_timer_alarm_deadline(timer_id)
        .expect("the dequeue re-arms the advanced period");
    assert!(
        armed_after_dequeue.saturating_duration_since(armed_after_arming) > TimeSpan::ZERO,
        "the dequeue must re-arm the advanced period: armed={armed_after_arming:?} now={:?}",
        monotonic_time()
    );
}

/// The dequeue sampling gate admits exactly the notifications the manager
/// will accept: a stale generation, a timer whose clock ignores the CPU
/// totals, and an unknown timer never need the sample.
#[def_test]
fn dequeue_cpu_time_gate_matches_the_manager_admission() {
    let mut manager = ProcessTimerManager::new(1);
    let timer_id = manager
        .create_posix_timer(
            CLOCK_PROCESS_CPUTIME_ID as i32,
            PosixTimerCreateNotify::Signal {
                signo: Signo::SIGALRM,
                target_tid: None,
                value: PosixTimerSigValue::TimerId,
            },
        )
        .unwrap();
    manager
        .set_posix_timer(
            timer_id,
            false,
            TimeSpan::from_nanos(3),
            TimeSpan::from_nanos(2),
            TimeSpan::ZERO,
            TimeSpan::ZERO,
        )
        .unwrap();
    let delivery = manager
        .poll_cpu_timers(TimeSpan::from_nanos(2), TimeSpan::ZERO)
        .into_iter()
        .next()
        .expect("timer delivery");
    let TimerDelivery::Process(TimerSignal::Posix { signal_seq, .. }) = delivery else {
        panic!("expected a POSIX timer signal");
    };
    assert!(manager.posix_timer_dequeue_needs_cpu_times(timer_id, signal_seq));

    // Re-arming bumps the generation: an in-flight notification from the
    // previous one is dropped by the manager, so the dequeue path must not
    // pay for the sample either.
    manager
        .set_posix_timer(
            timer_id,
            false,
            TimeSpan::from_nanos(3),
            TimeSpan::from_nanos(2),
            TimeSpan::from_nanos(2),
            TimeSpan::ZERO,
        )
        .unwrap();
    assert!(!manager.posix_timer_dequeue_needs_cpu_times(timer_id, signal_seq));

    // A wall-clock timer never consumes the sampled totals: assert with
    // its own generation so the clock check, not the generation check,
    // decides the answer.
    let wall_clock_id = manager
        .create_posix_timer(
            CLOCK_MONOTONIC as i32,
            PosixTimerCreateNotify::Signal {
                signo: Signo::SIGRTMIN,
                target_tid: None,
                value: PosixTimerSigValue::TimerId,
            },
        )
        .unwrap();
    let wall_clock_seq = manager
        .posix_timer_signal_seq(wall_clock_id)
        .expect("live timer");
    assert!(!manager.posix_timer_dequeue_needs_cpu_times(wall_clock_id, wall_clock_seq));

    // Unknown timers are not sampled.
    assert!(!manager.posix_timer_dequeue_needs_cpu_times(i32::MAX, signal_seq));
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use khal::time::monotonic_time;
use ksignal::Signo;
use ktime_types::{TaskCputime, TimeSpan};
use linux_raw_sys::general::{CLOCK_MONOTONIC, CLOCK_PROCESS_CPUTIME_ID};
use posix_types::ITimerType;
use unittest::def_test;

use super::ProcessTimerManager;
use crate::{
    PosixTimerConfig, TimerDelivery, TimerSignal,
    posix_timer::{PosixTimerCreateNotify, PosixTimerSigValue},
};

struct InactiveOrigin;

impl ksignal::TimerSignalOrigin for InactiveOrigin {
    fn complete(&self, _: i32, _: u64) -> ksignal::SignalDequeueAction {
        ksignal::SignalDequeueAction::Drop
    }
}

fn configuration(
    clock_id: i32,
    notify: PosixTimerCreateNotify,
) -> kerrno::KResult<PosixTimerConfig> {
    PosixTimerConfig::new(
        clock_id,
        notify,
        alloc::sync::Weak::<InactiveOrigin>::new(),
        ksignal::TimerSignalTarget::process(alloc::sync::Weak::new()),
    )
}

fn cpu_sample(utime_ns: u64, stime_ns: u64, runtime_ns: u64) -> TaskCputime {
    TaskCputime {
        utime: utime_ns,
        stime: stime_ns,
        sum_exec_runtime: runtime_ns,
    }
}

#[def_test]
fn configuration_rejects_unsupported_clock() {
    assert!(matches!(
        configuration(-1, PosixTimerCreateNotify::None),
        Err(kerrno::KError::InvalidInput)
    ));
}

#[def_test]
fn process_cpu_posix_timer_uses_scheduler_runtime() {
    let mut manager = ProcessTimerManager::new(1);
    let timer_id = manager
        .create_posix_timer(
            configuration(
                CLOCK_PROCESS_CPUTIME_ID as i32,
                PosixTimerCreateNotify::Signal {
                    signo: Signo::SIGALRM,
                    target_tid: None,
                    value: PosixTimerSigValue::TimerId,
                },
            )
            .unwrap(),
        )
        .unwrap();
    manager
        .set_posix_timer(
            timer_id,
            false,
            TimeSpan::ZERO,
            TimeSpan::from_nanos(5),
            TimeSpan::from_nanos(10),
        )
        .unwrap()
        .finish();

    assert!(manager.poll_cpu_timers(cpu_sample(100, 100, 14)).is_empty());
    assert_eq!(manager.poll_cpu_timers(cpu_sample(0, 0, 15)).len(), 1);
}

#[def_test]
fn deleted_timer_rejects_completion_before_releasing_ownership() {
    let origin = alloc::sync::Arc::new(InactiveOrigin);
    let weak = alloc::sync::Arc::downgrade(&origin);
    let mut manager = ProcessTimerManager::new(1);
    let timer_id = manager
        .create_posix_timer(
            PosixTimerConfig::new(
                CLOCK_PROCESS_CPUTIME_ID as i32,
                PosixTimerCreateNotify::None,
                weak,
                ksignal::TimerSignalTarget::process(alloc::sync::Weak::new()),
            )
            .unwrap(),
        )
        .unwrap();
    let sequence = manager.posix_timer_signal_seq(timer_id).unwrap();
    let retired = manager.delete_posix_timer(timer_id).unwrap();
    assert_eq!(alloc::sync::Arc::weak_count(&origin), 1);
    assert!(
        manager
            .get_posix_timer(timer_id, TimeSpan::ZERO)
            .is_err()
    );
    assert!(!manager.on_timer_signal_dequeued(timer_id, sequence, TimeSpan::ZERO));
    assert!(matches!(
        manager.delete_posix_timer(timer_id),
        Err(kerrno::KError::InvalidInput)
    ));
    manager.next_posix_timer_id = timer_id;
    let successor = manager
        .create_posix_timer(
            configuration(
                CLOCK_PROCESS_CPUTIME_ID as i32,
                PosixTimerCreateNotify::None,
            )
            .unwrap(),
        )
        .unwrap();
    assert_eq!(successor, timer_id);
    assert!(!manager.on_timer_signal_dequeued(successor, sequence, TimeSpan::ZERO));
    drop(retired);
    assert_eq!(alloc::sync::Arc::weak_count(&origin), 0);
    assert!(
        manager
            .get_posix_timer(successor, TimeSpan::ZERO)
            .is_ok()
    );
    drop(manager.delete_posix_timer(successor).unwrap());
}

#[def_test]
fn cleared_timer_map_retains_owners_until_unlocked_release() {
    let origin = alloc::sync::Arc::new(InactiveOrigin);
    let mut manager = ProcessTimerManager::new(1);
    for _ in 0..2 {
        let weak = alloc::sync::Arc::downgrade(&origin);
        let timer_id = manager
            .create_posix_timer(
                PosixTimerConfig::new(
                    CLOCK_PROCESS_CPUTIME_ID as i32,
                    PosixTimerCreateNotify::None,
                    weak,
                    ksignal::TimerSignalTarget::process(alloc::sync::Weak::new()),
                )
                .unwrap(),
            )
            .unwrap();
        manager
            .set_posix_timer(
                timer_id,
                false,
                TimeSpan::ZERO,
                TimeSpan::from_nanos(10),
                TimeSpan::ZERO,
            )
            .unwrap()
            .finish();
    }
    let sequence = manager.posix_timer_signal_seq(1).unwrap();
    let next_sequence = manager.next_posix_signal_seq;
    assert!(manager.any_cpu_timer_armed());
    let retired = manager.clear_posix_timers();
    assert!(manager.posix_timers.is_empty());
    assert!(!manager.any_cpu_timer_armed());
    assert_eq!(manager.next_posix_signal_seq, next_sequence);
    assert_eq!(alloc::sync::Arc::weak_count(&origin), 2);
    assert!(!manager.on_timer_signal_dequeued(1, sequence, TimeSpan::ZERO));
    drop(manager.clear_posix_timers());
    assert_eq!(alloc::sync::Arc::weak_count(&origin), 2);
    drop(retired);
    assert_eq!(alloc::sync::Arc::weak_count(&origin), 0);
}

#[def_test]
fn generation_exhaustion_rejects_creation_without_consuming_identity() {
    let mut manager = ProcessTimerManager::new(1);
    manager.next_posix_signal_seq = Some(u64::MAX);
    let timer_id = manager
        .create_posix_timer(
            configuration(CLOCK_MONOTONIC as i32, PosixTimerCreateNotify::None).unwrap(),
        )
        .unwrap();
    assert_eq!(manager.posix_timer_signal_seq(timer_id), Some(u64::MAX));
    assert_eq!(manager.next_posix_signal_seq, None);
    let next_id = manager.next_posix_timer_id;
    let rejected = manager
        .create_posix_timer_with_id(
            configuration(CLOCK_MONOTONIC as i32, PosixTimerCreateNotify::None).unwrap(),
            |_| panic!("exhausted generation must reject before ID allocation"),
        )
        .unwrap_err();
    let (error, _config) = rejected.into_parts();
    assert_eq!(error, kerrno::KError::InvalidInput);
    assert_eq!(manager.next_posix_timer_id, next_id);
    assert_eq!(manager.posix_timers.len(), 1);
    drop(manager.delete_posix_timer(timer_id).unwrap());
    drop(manager.clear_posix_timers());
    assert_eq!(manager.next_posix_signal_seq, None);
}

#[def_test]
fn generation_exhaustion_preserves_armed_timer_and_allows_deletion() {
    let mut manager = ProcessTimerManager::new(1);
    let timer_id = manager
        .create_posix_timer(
            configuration(
                CLOCK_PROCESS_CPUTIME_ID as i32,
                PosixTimerCreateNotify::None,
            )
            .unwrap(),
        )
        .unwrap();
    manager.next_posix_signal_seq = Some(u64::MAX);
    assert!(
        manager
            .set_posix_timer(
                -1,
                false,
                TimeSpan::ZERO,
                TimeSpan::ZERO,
                TimeSpan::ZERO
            )
            .is_err()
    );
    assert_eq!(manager.next_posix_signal_seq, Some(u64::MAX));
    assert!(matches!(
        manager.set_posix_timer(
            timer_id,
            false,
            TimeSpan::ZERO,
            TimeSpan::MAX,
            TimeSpan::from_nanos(1),
        ),
        Err(kerrno::KError::InvalidInput)
    ));
    assert_eq!(manager.next_posix_signal_seq, Some(u64::MAX));
    manager
        .set_posix_timer(
            timer_id,
            false,
            TimeSpan::from_nanos(3),
            TimeSpan::from_nanos(10),
            TimeSpan::ZERO,
        )
        .unwrap()
        .finish();
    let before = manager
        .get_posix_timer(timer_id, TimeSpan::ZERO)
        .unwrap();
    assert!(matches!(
        manager.set_posix_timer(
            timer_id,
            false,
            TimeSpan::ZERO,
            TimeSpan::ZERO,
            TimeSpan::ZERO
        ),
        Err(kerrno::KError::InvalidInput)
    ));
    assert_eq!(
        manager
            .get_posix_timer(timer_id, TimeSpan::ZERO)
            .unwrap(),
        before
    );
    assert_eq!(manager.posix_timer_signal_seq(timer_id), Some(u64::MAX));
    drop(manager.delete_posix_timer(timer_id).unwrap());
    assert!(manager.posix_timers.is_empty());
}

#[def_test]
fn prepared_configuration_publishes_only_when_consumed() {
    let mut manager = ProcessTimerManager::new(1);
    let config = configuration(CLOCK_MONOTONIC as i32, PosixTimerCreateNotify::None).unwrap();
    let next_id = manager.next_posix_timer_id;
    let next_sequence = manager.next_posix_signal_seq;
    assert!(manager.posix_timers.is_empty());
    let timer_id = manager.create_posix_timer(config).unwrap();
    assert_eq!(timer_id, next_id);
    assert_eq!(manager.posix_timer_signal_seq(timer_id), next_sequence);
    assert!(!manager.any_cpu_timer_armed());
    drop(manager.delete_posix_timer(timer_id).unwrap());
    assert!(manager.posix_timers.is_empty());
}

#[def_test]
fn rejected_admission_returns_configuration_without_publishing() {
    let mut manager = ProcessTimerManager::new(1);
    let config = configuration(
        CLOCK_PROCESS_CPUTIME_ID as i32,
        PosixTimerCreateNotify::Signal {
            signo: Signo::SIGRTMIN,
            target_tid: Some(42),
            value: PosixTimerSigValue::TimerId,
        },
    )
    .unwrap();
    let next_id = manager.next_posix_timer_id;
    let next_sequence = manager.next_posix_signal_seq;
    let rejected = manager
        .create_posix_timer_with_id(config, |_| Err(kerrno::KError::InvalidInput))
        .unwrap_err();
    assert!(manager.posix_timers.is_empty());
    assert_eq!(manager.next_posix_timer_id, next_id);
    assert_eq!(manager.next_posix_signal_seq, next_sequence);
    assert!(!manager.any_cpu_timer_armed());

    let (error, config) = rejected.into_parts();
    assert_eq!(error, kerrno::KError::InvalidInput);
    let timer_id = manager.create_posix_timer(config).unwrap();
    assert_eq!(timer_id, next_id);
    assert_eq!(manager.posix_timer_signal_seq(timer_id), next_sequence);
    manager
        .set_posix_timer(
            timer_id,
            false,
            TimeSpan::ZERO,
            TimeSpan::from_nanos(1),
            TimeSpan::ZERO,
        )
        .unwrap()
        .finish();
    let deliveries = manager.poll_cpu_timers(cpu_sample(1, 0, 1));
    assert_eq!(deliveries.len(), 1);
    assert!(matches!(
        deliveries.as_slice(),
        [TimerDelivery::Thread {
            tid: 42,
            signal: TimerSignal::Posix {
                notification,
                ..
            },
        }] if notification.signo() == Signo::SIGRTMIN && notification.receipt().unwrap().timer_id() == timer_id
    ));
}

#[def_test]
fn test_any_cpu_timer_armed_tracks_arm_and_disarm() {
    let mut manager = ProcessTimerManager::new(1);
    assert!(!manager.any_cpu_timer_armed());

    // Wall-clock itimers never arm the CPU-timer gate.
    manager.set_itimer(
        ITimerType::Real,
        TimeSpan::from_nanos(10),
        TimeSpan::from_nanos(10),
        cpu_sample(0, 0, 0),
    );
    assert!(!manager.any_cpu_timer_armed());

    manager.set_itimer(
        ITimerType::Virtual,
        TimeSpan::ZERO,
        TimeSpan::from_nanos(10),
        cpu_sample(0, 0, 0),
    );
    assert!(manager.any_cpu_timer_armed());

    // One-shot expiry disarms through update(); the gate mirrors that.
    manager.poll_cpu_timers(cpu_sample(10, 0, 10));
    assert!(!manager.any_cpu_timer_armed());

    // Process-CPU POSIX timers participate in the gate.
    let timer_id = manager
        .create_posix_timer(
            configuration(
                CLOCK_PROCESS_CPUTIME_ID as i32,
                PosixTimerCreateNotify::Signal {
                    signo: Signo::SIGRTMIN,
                    target_tid: None,
                    value: PosixTimerSigValue::TimerId,
                },
            )
            .unwrap(),
        )
        .unwrap();
    manager
        .set_posix_timer(
            timer_id,
            false,
            TimeSpan::from_nanos(5),
            TimeSpan::from_nanos(5),
            TimeSpan::ZERO,
        )
        .unwrap()
        .finish();
    assert!(manager.any_cpu_timer_armed());
    drop(manager.delete_posix_timer(timer_id).unwrap());
    assert!(!manager.any_cpu_timer_armed());
}

#[def_test]
fn test_process_timer_manager_set_itimer_returns_previous_values() {
    let mut manager = ProcessTimerManager::new(1);

    let (old_interval, old_remained) = manager.set_itimer(
        ITimerType::Virtual,
        TimeSpan::from_nanos(11),
        TimeSpan::from_nanos(22),
        cpu_sample(0, 0, 0),
    );
    assert_eq!(old_interval.as_secs(), 0);
    assert_eq!(old_remained.as_secs(), 0);

    let (old_interval, old_remained) = manager.set_itimer(
        ITimerType::Virtual,
        TimeSpan::from_nanos(33),
        TimeSpan::from_nanos(44),
        cpu_sample(0, 0, 0),
    );
    assert_eq!(old_interval.subsec_nanos(), 11);
    assert_eq!(old_remained.subsec_nanos(), 22);

    let (interval, remained) = manager.get_itimer(ITimerType::Virtual, cpu_sample(0, 0, 0));
    assert_eq!(interval.subsec_nanos(), 33);
    assert_eq!(remained.subsec_nanos(), 44);
}

/// CPU-timer expiry consumes the raw tick split, never the adjusted view.
///
/// Review counterexample: raw `(utime, stime) = (1ms, 1ms)` against a 10ms
/// scheduler runtime adjusts to a 5ms/5ms split. A 4ms ITIMER_VIRTUAL or
/// ITIMER_PROF deadline must not fire at that raw sample — Linux
/// `posix-cpu-timers` feeds raw values into expiry, and the adjusted view is
/// reserved for times/getrusage/wait.
#[def_test]
fn cpu_timer_expiry_uses_raw_split_not_adjusted() {
    let mut manager = ProcessTimerManager::new(1);
    // Raw 1ms user + 1ms system against 10ms runtime: adjustment would scale
    // the split to 5ms/5ms (PROF sum 10ms), past both 4ms deadlines.
    let sample = cpu_sample(1_000_000, 1_000_000, 10_000_000);

    manager.set_itimer(
        ITimerType::Virtual,
        TimeSpan::ZERO,
        TimeSpan::from_millis(4),
        sample,
    );
    manager.set_itimer(
        ITimerType::Prof,
        TimeSpan::ZERO,
        TimeSpan::from_millis(4),
        sample,
    );
    // The deadlines sit on the raw clock: VIRTUAL at 1ms + 4ms = 5ms user
    // time, PROF at 2ms + 4ms = 6ms user+system. An adjusted now (5ms user)
    // would already reach the VIRTUAL deadline and fire early.
    assert!(manager.poll_cpu_timers(sample).is_empty());
    // The reported remaining is raw too: at 2ms raw user time the 5ms
    // deadline has 3ms left.
    let (_, remained) = manager.get_itimer(
        ITimerType::Virtual,
        cpu_sample(2_000_000, 1_000_000, 10_000_000),
    );
    assert_eq!(remained, TimeSpan::from_millis(3));

    // Positive control: once the raw counters reach the deadlines both fire
    // (user 5ms >= VIRTUAL 5ms; user + system 6ms >= PROF 6ms).
    let reached = cpu_sample(5_000_000, 1_000_000, 10_000_000);
    assert_eq!(manager.poll_cpu_timers(reached).len(), 2);
}

#[def_test]
fn test_posix_timer_signal_seq_invalidates_stale_signal() {
    let mut manager = ProcessTimerManager::new(1);
    let timer_id = manager
        .create_posix_timer(
            configuration(
                CLOCK_PROCESS_CPUTIME_ID as i32,
                PosixTimerCreateNotify::Signal {
                    signo: Signo::SIGRTMIN,
                    target_tid: None,
                    value: PosixTimerSigValue::TimerId,
                },
            )
            .unwrap(),
        )
        .unwrap();
    let (_, delivery) = manager
        .set_posix_timer(
            timer_id,
            false,
            TimeSpan::ZERO,
            TimeSpan::from_nanos(1),
            TimeSpan::ZERO,
        )
        .unwrap()
        .finish();
    assert!(delivery.is_none());

    let delivery = manager
        .poll_cpu_timers(cpu_sample(1, 0, 1))
        .into_iter()
        .next()
        .expect("timer delivery");
    let stale_signal_seq = match delivery {
        TimerDelivery::Process(TimerSignal::Posix { notification, .. }) => {
            notification.receipt().unwrap().signal_seq()
        }
        _ => panic!("expected a POSIX timer signal"),
    };

    manager.next_posix_signal_seq = Some(stale_signal_seq + (1u64 << 32));

    let _ = manager
        .set_posix_timer(
            timer_id,
            false,
            TimeSpan::ZERO,
            TimeSpan::from_nanos(10),
            TimeSpan::from_nanos(1),
        )
        .unwrap()
        .finish();
    assert!(!manager.on_timer_signal_dequeued(
        timer_id,
        stale_signal_seq,
        TimeSpan::from_nanos(1),
    ));
    let current_seq = manager.posix_timer_signal_seq(timer_id).unwrap();
    assert_eq!(current_seq as u32, stale_signal_seq as u32);
    assert_ne!(current_seq, stale_signal_seq);
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
            configuration(
                CLOCK_PROCESS_CPUTIME_ID as i32,
                PosixTimerCreateNotify::Signal {
                    signo: Signo::SIGALRM,
                    target_tid: None,
                    value: PosixTimerSigValue::TimerId,
                },
            )
            .unwrap(),
        )
        .unwrap();
    let (_, delivery) = manager
        .set_posix_timer(
            timer_id,
            false,
            TimeSpan::from_nanos(3),
            TimeSpan::from_nanos(2),
            TimeSpan::ZERO,
        )
        .unwrap()
        .finish();
    assert!(delivery.is_none());
    // The first interval produces the notification; the two intervals that
    // elapse afterwards are still uncollected when it is dequeued.
    let delivery = manager
        .poll_cpu_timers(cpu_sample(2, 0, 2))
        .into_iter()
        .next()
        .expect("timer delivery");
    let signal_seq = match delivery {
        TimerDelivery::Process(TimerSignal::Posix { notification, .. }) => {
            notification.receipt().unwrap().signal_seq()
        }
        _ => panic!("expected a POSIX timer signal"),
    };
    assert!(manager.on_timer_signal_dequeued(
        timer_id,
        signal_seq,
        TimeSpan::from_nanos(8),
    ));
    // Both elapsed intervals belong to the delivered notification.
    assert_eq!(manager.get_posix_timer_overrun(timer_id), Ok(2));
    // They were absorbed, so the timer stays quiet until the next interval.
    assert!(
        manager
            .poll_cpu_timers(cpu_sample(8, 0, 8))
            .is_empty()
    );
    assert_eq!(
        manager
            .poll_cpu_timers(cpu_sample(11, 0, 11))
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
            configuration(
                CLOCK_MONOTONIC as i32,
                PosixTimerCreateNotify::Signal {
                    signo: Signo::SIGRTMIN,
                    target_tid: None,
                    value: PosixTimerSigValue::TimerId,
                },
            )
            .unwrap(),
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
        )
        .unwrap()
        .finish();
    let TimerDelivery::Process(TimerSignal::Posix { notification, .. }) =
        delivery.expect("a past deadline expires on arming")
    else {
        panic!("expected a POSIX timer signal");
    };
    let signal_seq = notification.receipt().unwrap().signal_seq();
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
        manager.on_timer_signal_dequeued(timer_id, signal_seq, TimeSpan::ZERO,)
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
            configuration(
                CLOCK_PROCESS_CPUTIME_ID as i32,
                PosixTimerCreateNotify::Signal {
                    signo: Signo::SIGALRM,
                    target_tid: None,
                    value: PosixTimerSigValue::TimerId,
                },
            )
            .unwrap(),
        )
        .unwrap();
    manager
        .set_posix_timer(
            timer_id,
            false,
            TimeSpan::from_nanos(3),
            TimeSpan::from_nanos(2),
            TimeSpan::ZERO,
        )
        .unwrap()
        .finish();
    let delivery = manager
        .poll_cpu_timers(cpu_sample(2, 0, 2))
        .into_iter()
        .next()
        .expect("timer delivery");
    let TimerDelivery::Process(TimerSignal::Posix { notification, .. }) = delivery else {
        panic!("expected a POSIX timer signal");
    };
    let signal_seq = notification.receipt().unwrap().signal_seq();
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
        )
        .unwrap()
        .finish();
    assert!(!manager.posix_timer_dequeue_needs_cpu_times(timer_id, signal_seq));

    // A wall-clock timer never consumes the sampled totals: assert with
    // its own generation so the clock check, not the generation check,
    // decides the answer.
    let wall_clock_id = manager
        .create_posix_timer(
            configuration(
                CLOCK_MONOTONIC as i32,
                PosixTimerCreateNotify::Signal {
                    signo: Signo::SIGRTMIN,
                    target_tid: None,
                    value: PosixTimerSigValue::TimerId,
                },
            )
            .unwrap(),
        )
        .unwrap();
    let wall_clock_seq = manager
        .posix_timer_signal_seq(wall_clock_id)
        .expect("live timer");
    assert!(!manager.posix_timer_dequeue_needs_cpu_times(wall_clock_id, wall_clock_seq));

    // Unknown timers are not sampled.
    assert!(!manager.posix_timer_dequeue_needs_cpu_times(i32::MAX, signal_seq));
}

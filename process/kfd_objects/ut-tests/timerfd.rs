// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kpoll::IoEvents;
use unittest::def_test;

use super::*;

fn monotonic(span: TimeSpan) -> TimerDeadline {
    TimerDeadline::Monotonic(MonotonicInstant::from_span_since_origin(span))
}

#[def_test]
fn interrupted_timerfd_read_preserves_nonblocking_and_count_consumption() {
    let file = TimerFd::new_file(CLOCK_MONOTONIC, 0, kcred::initial_cred()).unwrap();
    let timerfd = TimerFd::from_file(&file).unwrap();
    let task = ktask::current();
    let mut bytes = [0; 8];
    let mut pos = 0;
    task.interrupt();
    let waiting = file.read_from(&mut bytes, &mut pos);
    task.clear_interrupt();
    assert_eq!(waiting, Err(KError::IoWaitInterrupted));
    file.set_nonblocking(true);
    task.interrupt();
    let empty = file.read_from(&mut bytes, &mut pos);
    let preserved = task.is_interrupted();
    task.clear_interrupt();
    assert_eq!(empty, Err(KError::WouldBlock));
    assert!(preserved);
    timerfd.timer.handler().inner.lock().expirations = 3;
    task.interrupt();
    let ready = file.read_from(&mut bytes, &mut pos);
    let preserved = task.is_interrupted();
    task.clear_interrupt();
    assert_eq!(ready, Ok(8));
    assert_eq!(u64::from_ne_bytes(bytes), 3);
    assert!(preserved);
    assert_eq!(
        file.read_from(&mut bytes, &mut pos),
        Err(KError::WouldBlock)
    );
}

#[def_test]
fn timerfd_expirations_after_pending_precede_signal_and_are_consumed_once() {
    use alloc::boxed::Box;
    use core::{
        future::Future,
        task::{Context, Poll, Waker},
    };
    let timerfd = TimerFd::new(CLOCK_MONOTONIC);
    let task = ktask::current();
    task.clear_interrupt();
    let mut read = Box::pin(poll_file_io(timerfd.as_ref(), IoEvents::IN, false, || {
        timerfd.read_expirations()
    }));
    let mut cx = Context::from_waker(Waker::noop());
    assert_eq!(read.as_mut().poll(&mut cx), Poll::Pending);
    // Establish completed expirations without relying on timer IRQ timing.
    timerfd.timer.handler().inner.lock().expirations = 5;
    timerfd.timer.handler().poll_rx.wake();
    task.interrupt();
    let result = read.as_mut().poll(&mut cx);
    task.clear_interrupt();
    assert_eq!(result, Poll::Ready(Ok(5)));
    drop(read);
    assert_eq!(timerfd.read_expirations(), Err(KError::WouldBlock));
}

#[def_test(serial)]
fn poll_before_irq_preserves_waiter_notification() {
    use alloc::task::Wake;
    use core::{
        sync::atomic::{AtomicUsize, Ordering},
        task::Waker,
    };

    struct WakeCount(AtomicUsize);
    impl Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    for interval in [TimeSpan::ZERO, TimeSpan::from_millis(5)] {
        let tfd = TimerFd::new(CLOCK_MONOTONIC);
        let counters = [0; 2].map(|_| Arc::new(WakeCount(AtomicUsize::new(0))));
        let registrations = counters.each_ref().map(|counter| {
            tfd.timer
                .handler()
                .poll_rx
                .register(&Waker::from(counter.clone()))
                .unwrap()
        });
        let pin = kspin::NoPreempt::new();
        {
            let _irq = kspin::NoPreemptIrqSave::new();
            assert!(!tfd.poll().contains(IoEvents::IN));
            let deadline = monotonic_time();
            // Construct the due-but-undelivered state without taking the
            // sleepable settime mutex while IRQ delivery is masked.
            {
                let mut inner = tfd.timer.handler().inner.lock();
                inner.deadline = Some(TimerDeadline::Monotonic(deadline));
                inner.interval = interval;
            }
            // SAFETY: initialized task context, with CPU migration and IRQ
            // delivery disabled. The owned handler retains independent data
            // and only locks IRQ-safe state and wakes these atomic counters.
            unsafe { tfd.timer.start(deadline) }.unwrap();
            assert!(tfd.poll().contains(IoEvents::IN));
            assert!(counters.iter().all(|c| c.0.load(Ordering::Relaxed) == 0));
        }
        // No read/gettime may consume readiness or supply another wakeup.
        let limit = monotonic_time() + TimeSpan::from_secs(2);
        while counters.iter().any(|c| c.0.load(Ordering::Relaxed) == 0) && monotonic_time() < limit
        {
            core::hint::spin_loop();
        }
        tfd.timer.cancel_sync();
        let all_woken = counters.iter().all(|c| c.0.load(Ordering::Relaxed) > 0);
        let is_readable = tfd.poll().contains(IoEvents::IN);
        drop(pin);
        drop(registrations);
        drop(tfd);
        assert!(
            all_woken,
            "poll must not suppress the timer IRQ notification"
        );
        assert!(is_readable, "notification must not consume expirations");
    }
}

#[def_test]
fn test_timerfd_last_owner_drop_releases_periodic_storage() {
    use alloc::task::Wake;
    use core::{
        sync::atomic::{AtomicUsize, Ordering},
        task::Waker,
    };

    struct WakeCount(AtomicUsize);
    impl Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
    let tfd = TimerFd::new(CLOCK_MONOTONIC);
    tfd.settime(false, TimeSpan::from_secs(60), TimeSpan::from_secs(1))
        .unwrap();
    let owner = Arc::downgrade(&tfd);
    let wake = Arc::new(WakeCount(AtomicUsize::new(0)));
    let registration = tfd
        .timer
        .handler()
        .poll_rx
        .register(&Waker::from(wake.clone()))
        .unwrap();
    assert_eq!(Arc::strong_count(&wake), 2);
    drop(tfd);
    assert!(owner.upgrade().is_none());
    // PollSet destruction wakes and releases its retained waker. Keeping the
    // registration alive must not keep the handler or FD storage alive.
    assert_eq!(wake.0.load(Ordering::Relaxed), 1);
    assert_eq!(Arc::strong_count(&wake), 1);
    assert!(!registration.cancel());
}

#[def_test(serial)]
fn test_timerfd_periodic_irq_and_disarm() {
    let tfd = TimerFd::new(CLOCK_MONOTONIC);
    tfd.settime(false, TimeSpan::from_millis(2), TimeSpan::from_millis(2))
        .unwrap();
    let limit = monotonic_time() + TimeSpan::from_secs(2);
    while tfd.timer.handler().inner.lock().expirations == 0 && monotonic_time() < limit {
        ktask::yield_now();
    }
    // Inspect without poll/tick: only actual timer IRQ delivery increments it.
    assert!(tfd.timer.handler().inner.lock().expirations > 0);
    ktask::sleep(TimeSpan::from_millis(6));
    assert!(tfd.read_expirations().unwrap() >= 2);
    tfd.settime(false, TimeSpan::ZERO, TimeSpan::ZERO).unwrap();
    ktask::sleep(TimeSpan::from_millis(10));
    let inner = tfd.timer.handler().inner.lock();
    assert_eq!(inner.expirations, 0);
    assert!(inner.deadline.is_none());
}

#[def_test]
fn test_timerfd_gettime_advances_unread_periods() {
    let tfd = TimerFd::new(CLOCK_MONOTONIC);
    tfd.settime(true, TimeSpan::from_nanos(1), TimeSpan::from_secs(1))
        .unwrap();
    let (_, remaining) = tfd.gettime();
    assert!(remaining > TimeSpan::ZERO);
    assert!(remaining <= TimeSpan::from_secs(1));
    assert!(tfd.read_expirations().unwrap() > 0);
}

#[def_test(serial)]
fn test_timerfd_tiny_interval_waits_for_read_before_rearming() {
    let tfd = TimerFd::new(CLOCK_MONOTONIC);
    tfd.settime(false, TimeSpan::from_millis(2), TimeSpan::from_nanos(1))
        .unwrap();
    let limit = monotonic_time() + TimeSpan::from_secs(2);
    while tfd.timer.handler().inner.lock().expirations == 0 && monotonic_time() < limit {
        ktask::yield_now();
    }
    let first_count = tfd.timer.handler().inner.lock().expirations;
    assert!(first_count > 0);
    ktask::sleep(TimeSpan::from_millis(5));
    assert_eq!(tfd.timer.handler().inner.lock().expirations, first_count);
    assert!(tfd.read_expirations().unwrap() > first_count);
    tfd.settime(false, TimeSpan::ZERO, TimeSpan::ZERO).unwrap();
}

#[def_test]
fn test_timerfd_settime_returns_advanced_old_period() {
    let tfd = TimerFd::new(CLOCK_MONOTONIC);
    tfd.settime(true, TimeSpan::from_nanos(1), TimeSpan::from_secs(1))
        .unwrap();
    // Model an unread overdue period without relying on IRQ timing.
    tfd.timer.handler().inner.lock().deadline = Some(monotonic(TimeSpan::from_nanos(1)));
    let (interval, remaining) = tfd.settime(false, TimeSpan::ZERO, TimeSpan::ZERO).unwrap();
    assert_eq!(interval, TimeSpan::from_secs(1));
    assert!(remaining > TimeSpan::ZERO);
    assert!(remaining <= interval);
    assert!(!tfd.poll().contains(IoEvents::IN));
}

#[def_test]
fn test_timerfd_large_monotonic_deadline_is_not_truncated() {
    let tfd = TimerFd::new(CLOCK_MONOTONIC);
    let value = TimeSpan::from_secs(u64::MAX);
    tfd.settime(true, value, TimeSpan::ZERO).unwrap();
    assert_eq!(
        tfd.timer.handler().inner.lock().deadline,
        Some(monotonic(value))
    );
    assert!(!tfd.poll().contains(IoEvents::IN));
}

#[def_test]
fn test_timerfd_concurrent_programming_then_disarm() {
    let tfd = TimerFd::new(CLOCK_MONOTONIC);
    let workers = [false, true].map(|periodic| {
        let tfd = tfd.clone();
        ktask::spawn(move || {
            for _ in 0..32 {
                tfd.settime(
                    false,
                    TimeSpan::from_secs(60),
                    if periodic {
                        TimeSpan::from_secs(1)
                    } else {
                        TimeSpan::ZERO
                    },
                )
                .unwrap();
                tfd.settime(false, TimeSpan::ZERO, TimeSpan::ZERO).unwrap();
            }
        })
    });
    for worker in workers {
        assert_eq!(worker.join(), 0);
    }
    tfd.settime(false, TimeSpan::ZERO, TimeSpan::ZERO).unwrap();
    assert_eq!(tfd.gettime(), (TimeSpan::ZERO, TimeSpan::ZERO));
    assert!(!tfd.poll().contains(IoEvents::IN));
    let owner = Arc::downgrade(&tfd);
    drop(tfd);
    assert!(owner.upgrade().is_none());
}

#[def_test]
fn test_timerfd_creation() {
    let tfd = TimerFd::new(CLOCK_MONOTONIC);
    assert!(!tfd.poll().contains(IoEvents::IN));
}

#[def_test]
fn test_timerfd_disarmed_poll() {
    let tfd = TimerFd::new(CLOCK_MONOTONIC);
    assert!(!tfd.poll().contains(IoEvents::IN));
}

#[def_test]
fn test_timerfd_gettime_disarmed() {
    let tfd = TimerFd::new(CLOCK_MONOTONIC);
    let (interval, remaining) = tfd.gettime();
    assert!(interval.is_zero());
    assert!(remaining.is_zero());
}

#[def_test]
fn test_timerfd_nonblocking() {
    let file =
        TimerFd::new_file(CLOCK_MONOTONIC, 0, kcred::initial_cred()).expect("timerfd file opens");
    assert!(!file.is_nonblocking());
    file.set_nonblocking(true);
    assert!(file.is_nonblocking());
    file.set_nonblocking(false);
    assert!(!file.is_nonblocking());
}

#[def_test]
fn test_timerfd_settime_disarm() {
    let tfd = TimerFd::new(CLOCK_MONOTONIC);
    let (old_interval, old_remaining) = tfd
        .settime(false, TimeSpan::from_secs(10), TimeSpan::ZERO)
        .unwrap();
    assert!(old_interval.is_zero());
    assert!(old_remaining.is_zero());

    let (old_interval, _) = tfd.settime(false, TimeSpan::ZERO, TimeSpan::ZERO).unwrap();
    assert!(old_interval.is_zero());

    let (interval, remaining) = tfd.gettime();
    assert!(interval.is_zero());
    assert!(remaining.is_zero());
}

#[def_test]
fn test_timerfd_invalid_deadline_preserves_state() {
    let tfd = TimerFd::new(linux_raw_sys::general::CLOCK_REALTIME);
    let deadline = TimerDeadline::Realtime(SystemTime::from_unix_seconds(100));
    {
        let mut inner = tfd.timer.handler().inner.lock();
        inner.deadline = Some(deadline);
        inner.interval = TimeSpan::from_secs(2);
        inner.expirations = 3;
    }

    assert_eq!(
        tfd.settime(true, TimeSpan::MAX, TimeSpan::ZERO),
        Err(KError::InvalidInput)
    );

    let inner = tfd.timer.handler().inner.lock();
    assert_eq!(inner.deadline, Some(deadline));
    assert_eq!(inner.interval, TimeSpan::from_secs(2));
    assert_eq!(inner.expirations, 3);
}

#[def_test]
fn test_timerfd_already_expired() {
    let tfd = TimerFd::new(CLOCK_MONOTONIC);
    tfd.settime(false, TimeSpan::from_nanos(1), TimeSpan::ZERO)
        .unwrap();
    assert!(tfd.poll().contains(IoEvents::IN));
}

#[def_test]
fn test_timerfd_read_small_buffer() {
    let file =
        TimerFd::new_file(CLOCK_MONOTONIC, 0, kcred::initial_cred()).expect("timerfd file opens");
    let mut small_out = [0u8; 4];
    let mut pos = 0;
    assert_eq!(
        file.read_from(&mut small_out, &mut pos),
        Err(KError::InvalidInput)
    );
}

#[def_test]
fn test_timerfd_write_returns_error() {
    let file =
        TimerFd::new_file(CLOCK_MONOTONIC, 0, kcred::initial_cred()).expect("timerfd file opens");
    let data = b"test1234";
    let mut pos = 0;
    assert!(file.write_from(data, &mut pos).is_err());
}

#[def_test]
fn test_timerfd_tick_oneshot() {
    let mut inner = TimerFdInner {
        expirations: 0,
        interval: TimeSpan::ZERO,
        deadline: Some(monotonic(TimeSpan::from_secs(100))),
    };
    assert!(!inner.tick(monotonic(TimeSpan::from_secs(99))));
    assert_eq!(inner.expirations, 0);
    assert!(inner.deadline.is_some());

    assert!(inner.tick(monotonic(TimeSpan::from_secs(100))));
    assert_eq!(inner.expirations, 1);
    assert!(inner.deadline.is_none());
}

#[def_test]
fn test_timerfd_tick_periodic() {
    let mut inner = TimerFdInner {
        expirations: 0,
        interval: TimeSpan::from_millis(100),
        deadline: Some(monotonic(TimeSpan::from_secs(10))),
    };
    assert!(inner.tick(monotonic(TimeSpan::from_millis(10_350))));
    assert_eq!(inner.expirations, 4);
    assert_eq!(
        inner.deadline,
        Some(monotonic(TimeSpan::from_millis(10_400)))
    );
}

#[def_test]
fn test_timerfd_tick_periodic_accumulates() {
    let mut inner = TimerFdInner {
        expirations: 2,
        interval: TimeSpan::from_millis(100),
        deadline: Some(monotonic(TimeSpan::from_secs(10))),
    };
    assert!(inner.tick(monotonic(TimeSpan::from_millis(10_050))));
    assert_eq!(inner.expirations, 3);
}

#[def_test]
fn test_timerfd_settime_rearm_returns_old() {
    let tfd = TimerFd::new(CLOCK_MONOTONIC);
    tfd.settime(false, TimeSpan::from_secs(10), TimeSpan::from_secs(1))
        .unwrap();

    let (old_interval, old_remaining) = tfd
        .settime(false, TimeSpan::from_secs(5), TimeSpan::ZERO)
        .unwrap();
    assert_eq!(old_interval, TimeSpan::from_secs(1));
    assert!(old_remaining > TimeSpan::ZERO);
}

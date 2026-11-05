// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::task::Wake;
use core::{future::Future, pin::pin, sync::atomic::AtomicUsize, task::Waker};

use ktime_types::{BoottimeInstant, SystemTime, TimeSpan};
use unittest::{assert, assert_eq, def_test};

use super::*;

fn mono(seconds: u64) -> MonotonicInstant {
    MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(seconds))
}

#[derive(Default)]
struct AlarmWakeCounter(AtomicUsize);

impl Wake for AlarmWakeCounter {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, AtomicOrdering::Relaxed);
    }
}

#[def_test]
fn alarm_request_before_registration_survives_until_poll() {
    let wake = AlarmWake::new();
    wake.notify();
    wake.notify();
    let mut context = Context::from_waker(Waker::noop());
    assert!(wake.poll_changed(&mut context).is_ready());
    assert!(wake.poll_changed(&mut context).is_pending());
}

#[def_test]
fn alarm_wait_registers_again_after_every_consumed_wake() {
    let wake = AlarmWake::new();
    let counter = Arc::new(AlarmWakeCounter::default());
    let waker = Waker::from(counter.clone());
    let mut context = Context::from_waker(&waker);
    for expected in 1..=3 {
        assert!(wake.poll_changed(&mut context).is_pending());
        wake.notify();
        assert_eq!(counter.0.load(AtomicOrdering::Relaxed), expected);
        wake.notify();
        assert_eq!(counter.0.load(AtomicOrdering::Relaxed), expected);
        assert!(wake.poll_changed(&mut context).is_ready());
    }
}

#[def_test]
fn cancelled_alarm_wait_preserves_request_for_replacement_wait() {
    let wake = AlarmWake::new();
    let old_counter = Arc::new(AlarmWakeCounter::default());
    let old_waker = Waker::from(old_counter.clone());
    let mut old_context = Context::from_waker(&old_waker);
    {
        let mut wait = pin!(wake.changed());
        assert!(wait.as_mut().poll(&mut old_context).is_pending());
    }
    wake.notify();
    assert_eq!(old_counter.0.load(AtomicOrdering::Relaxed), 1);
    let counter = Arc::new(AlarmWakeCounter::default());
    let waker = Waker::from(counter.clone());
    let mut context = Context::from_waker(&waker);
    let mut replacement = pin!(wake.changed());
    assert!(replacement.as_mut().poll(&mut context).is_ready());
    assert!(wake.poll_changed(&mut context).is_pending());
    wake.notify();
    assert_eq!(counter.0.load(AtomicOrdering::Relaxed), 1);
    assert_eq!(old_counter.0.load(AtomicOrdering::Relaxed), 1);
}

#[def_test]
fn alarm_wait_ready_does_not_depend_on_other_wait_branches() {
    let wake = AlarmWake::new();
    let mut wait = pin!(async {
        select_biased! {
            _ = wake.changed().fuse() => true,
            _ = core::future::pending::<()>().fuse() => false,
        }
    });
    let counter = Arc::new(AlarmWakeCounter::default());
    let waker = Waker::from(counter.clone());
    let mut context = Context::from_waker(&waker);
    assert!(wait.as_mut().poll(&mut context).is_pending());
    wake.notify();
    assert_eq!(counter.0.load(AtomicOrdering::Relaxed), 1);
    assert_eq!(wait.as_mut().poll(&mut context), Poll::Ready(true));
}

#[def_test]
fn offset_change_reorders_domains_without_reordering_heaps() {
    let mut queues = AlarmQueues::default();
    let monotonic = TimerInstant::Monotonic(mono(50));
    let boottime = TimerInstant::Boottime(BoottimeInstant::from_span_since_origin(
        TimeSpan::from_secs(80),
    ));
    let realtime = TimerInstant::Realtime(SystemTime::from_unix_seconds(1000));
    for (pid, deadline) in [(1, monotonic), (2, boottime), (3, realtime)] {
        queues.queue(deadline).unwrap().push(Entry {
            deadline,
            pid,
            ticket: Arc::new(AlarmTicket::new()),
        });
    }
    let convert = |boot_offset: u64, real_offset: u64| {
        queues
            .earliest_with(|deadline| match deadline {
                TimerInstant::Monotonic(deadline) => Some(deadline),
                TimerInstant::Boottime(_) => Some(mono(80 - boot_offset)),
                TimerInstant::Realtime(_) => Some(mono(1000 - real_offset)),
                _ => None,
            })
            .unwrap()
            .1
    };
    assert_eq!(convert(0, 900), monotonic);
    assert_eq!(convert(60, 900), boottime);
    assert_eq!(convert(0, 990), realtime);
    assert_eq!(queues.boottime.peek().unwrap().deadline, boottime);
    assert!(
        queues
            .queue(TimerInstant::ProcessCpu(
                ktime_types::ProcessCpuInstant::ORIGIN
            ))
            .is_none()
    );
}

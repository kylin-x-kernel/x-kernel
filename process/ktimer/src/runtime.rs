// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Global alarm runtime backend.
//!
//! This module owns the background wait queue that wakes process timer owners
//! when a wall-clock deadline expires. It does not interpret timer policy; it
//! only tracks `(deadline, pid)` alarm entries and invokes the registered
//! expired-owner callback.

extern crate alloc;

use alloc::{borrow::ToOwned, collections::binary_heap::BinaryHeap, sync::Arc};
use core::{
    cmp::Ordering,
    future::poll_fn,
    sync::atomic::{AtomicBool, Ordering as AtomicOrdering},
    task::{Context, Poll},
};

use futures_util::{FutureExt, select_biased, task::AtomicWaker};
use khal::time::monotonic_time;
use klazy::Once;
use ksync::{Mutex, static_lock};
use ktask::future::{block_on, timeout_at};
use ktime_types::MonotonicInstant;

use crate::{Pid, interval_timer::TimerInstant};

struct Entry {
    deadline: TimerInstant,
    pid: Pid,
    ticket: Arc<AlarmTicket>,
}

impl PartialEq for Entry {
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline && self.pid == other.pid
    }
}

impl Eq for Entry {}

impl PartialOrd for Entry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Entry {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .deadline
            .cmp(&self.deadline)
            .then_with(|| other.pid.cmp(&self.pid))
    }
}

#[derive(Default)]
struct AlarmQueues {
    monotonic: BinaryHeap<Entry>,
    realtime: BinaryHeap<Entry>,
    boottime: BinaryHeap<Entry>,
}
impl AlarmQueues {
    fn queue(&mut self, deadline: TimerInstant) -> Option<&mut BinaryHeap<Entry>> {
        match deadline {
            TimerInstant::Monotonic(_) => Some(&mut self.monotonic),
            TimerInstant::Realtime(_) => Some(&mut self.realtime),
            TimerInstant::Boottime(_) => Some(&mut self.boottime),
            TimerInstant::ProcessCpu(_) => None,
        }
    }

    fn earliest(&self) -> Option<(MonotonicInstant, TimerInstant)> {
        self.earliest_with(|deadline| match deadline {
            TimerInstant::Monotonic(deadline) => Some(deadline),
            TimerInstant::Realtime(deadline) => {
                Some(ktime::realtime_deadline_to_monotonic(deadline))
            }
            TimerInstant::Boottime(deadline) => {
                Some(ktime::boottime_deadline_to_monotonic(deadline))
            }
            TimerInstant::ProcessCpu(_) => None,
        })
    }

    fn earliest_with(
        &self,
        convert: impl Fn(TimerInstant) -> Option<MonotonicInstant>,
    ) -> Option<(MonotonicInstant, TimerInstant)> {
        [&self.monotonic, &self.realtime, &self.boottime]
            .into_iter()
            .filter_map(|queue| queue.peek())
            .filter_map(|entry| convert(entry.deadline).map(|runtime| (runtime, entry.deadline)))
            .min_by_key(|(runtime, _)| *runtime)
    }
}
static_lock! {
    static ALARM_LIST: Mutex<AlarmQueues> = Mutex::new(AlarmQueues {
        monotonic: BinaryHeap::new(), realtime: BinaryHeap::new(), boottime: BinaryHeap::new(),
    });
}
static ALARM_WAKE: AlarmWake = AlarmWake::new();
static ALARM_TASK_INIT: Once<()> = Once::new();
static EXPIRED_TASK_HANDLER: Once<fn(Pid)> = Once::new();

/// Coalesced rescan requests for the single alarm task, independent of wait lifetime.
struct AlarmWake {
    needs_rescan: AtomicBool,
    waker: AtomicWaker,
}

impl AlarmWake {
    const fn new() -> Self {
        Self {
            needs_rescan: AtomicBool::new(false),
            waker: AtomicWaker::new(),
        }
    }

    /// Publishes after the alarm queues are updated and unlocked.
    ///
    /// This stores no dynamically allocated listener. The sole consumer uses
    /// block_on's task-bound waker; its clone/wake operations allocate no owner.
    fn notify(&self) {
        self.needs_rescan.store(true, AtomicOrdering::Release);
        self.waker.wake();
    }

    fn poll_changed(&self, context: &mut Context<'_>) -> Poll<()> {
        self.waker.register(context.waker());
        if self.needs_rescan.swap(false, AtomicOrdering::AcqRel) {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }

    /// Wait cancellation preserves requests; every poll renews wake registration.
    async fn changed(&self) {
        poll_fn(|context| self.poll_changed(context)).await
    }
}

pub(crate) struct AlarmTicket(AtomicBool);
impl AlarmTicket {
    fn new() -> Self {
        Self(AtomicBool::new(true))
    }

    pub(crate) fn is_queued(&self) -> bool {
        self.0.load(AtomicOrdering::Acquire)
    }

    pub(crate) fn consume(&self) {
        self.0.store(false, AtomicOrdering::Release);
    }
}

pub(super) fn enqueue_alarm(deadline: TimerInstant, pid: Pid) -> Option<Arc<AlarmTicket>> {
    let mut guard = ALARM_LIST.lock();
    let queue = guard.queue(deadline)?;
    let should_wake = queue.peek().is_none_or(|it| it.deadline > deadline);
    let ticket = Arc::new(AlarmTicket::new());
    queue.push(Entry {
        deadline,
        pid,
        ticket: ticket.clone(),
    });
    drop(guard);
    if should_wake {
        ALARM_WAKE.notify();
    }
    Some(ticket)
}

/// Registers the callback used to handle expired timer owners.
///
/// The handler receives the PID whose timers must be polled; `kprocess`
/// registers it during process-subsystem init. Only the first registration
/// takes effect (`Once`).
pub fn register_expired_task_handler(handler: fn(Pid)) {
    EXPIRED_TASK_HANDLER.call_once(|| handler);
}

async fn alarm_task() {
    loop {
        let record_progress = ksignal::process_one_ready_timer_record();
        // Clock listeners precede snapshots; alarm rescan requests are persistent.
        let new_timer = ALARM_WAKE.changed();
        let realtime = ktime::clock_change_listener(ktime::ClockDomain::Realtime);
        let boottime = ktime::clock_change_listener(ktime::ClockDomain::Boottime);
        let (next, expired) = {
            let mut queues = ALARM_LIST.lock();
            let next = queues.earliest();
            let expired = next.and_then(|(deadline, original)| {
                if deadline <= monotonic_time() {
                    queues.queue(original).and_then(BinaryHeap::pop)
                } else {
                    None
                }
            });
            (next, expired)
        };
        if let Some(entry) = expired {
            entry.ticket.consume();
            if let Some(handler) = EXPIRED_TASK_HANDLER.get() {
                handler(entry.pid);
            }
            continue;
        }
        if record_progress {
            continue;
        }
        // Clock changes invalidate conversions, never original heap ordering.
        // A registration failure falls back to a bounded retry, not a busy loop.
        let failed = select_biased! {
            _ = timeout_at(next.map(|(deadline, _)| deadline), new_timer).fuse() => false,
            result = realtime.fuse() => result.is_err(),
            result = boottime.fuse() => result.is_err(),
        };
        if failed {
            ktask::future::sleep(ktime_types::TimeSpan::from_millis(1)).await;
        }
    }
}

/// Spawns the global alarm task that waits on the earliest queued
/// deadline across the three wall-clock domains and invokes the owner handler.
///
/// Repeated calls are idempotent: the wake registration has exactly one consumer.
/// The first call runs during kernel/process bring-up; the task runs for the
/// lifetime of the kernel. Task creation retains its existing allocation policy.
pub fn spawn_alarm_task() {
    ALARM_TASK_INIT.call_once(|| {
        ksignal::register_timer_record_waker(|| ALARM_WAKE.notify());
        ktask::spawn_raw(
            || block_on(alarm_task()),
            "alarm_task".to_owned(),
            kbuild_config::TASK_STACK_SIZE,
        );
    });
}

unittest::path_mod_test!(tests, "../ut-tests/runtime.rs");

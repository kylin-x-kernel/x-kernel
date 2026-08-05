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

use alloc::{borrow::ToOwned, collections::binary_heap::BinaryHeap};
use core::cmp::Ordering;

use event_listener::{Event, listener};
use khal::time::monotonic_time;
use klazy::Once;
use ksync::{Mutex, static_lock};
use ktask::future::{block_on, timeout_at};
use ktime_types::MonotonicInstant;

use crate::Pid;

struct Entry {
    deadline: MonotonicInstant,
    pid: Pid,
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

static_lock! {
    static ALARM_LIST: Mutex<BinaryHeap<Entry>> = Mutex::new(BinaryHeap::new());
}

static EVENT_NEW_TIMER: Event = Event::new();
static EXPIRED_TASK_HANDLER: Once<fn(Pid)> = Once::new();

pub(super) fn enqueue_alarm(deadline: MonotonicInstant, pid: Pid) {
    let mut guard = ALARM_LIST.lock();
    let should_wake = guard.peek().is_none_or(|it| it.deadline > deadline);
    guard.push(Entry { deadline, pid });
    drop(guard);
    if should_wake {
        EVENT_NEW_TIMER.notify(1);
    }
}

/// Registers the callback used to handle expired timer owners.
pub fn register_expired_task_handler(handler: fn(Pid)) {
    EXPIRED_TASK_HANDLER.call_once(|| handler);
}

async fn alarm_task() {
    loop {
        let next_deadline = {
            let guard = ALARM_LIST.lock();
            guard.peek().map(|entry| entry.deadline)
        };

        let Some(deadline) = next_deadline else {
            listener!(EVENT_NEW_TIMER => listener);
            if !ALARM_LIST.lock().is_empty() {
                continue;
            }
            listener.await;
            continue;
        };

        let now = monotonic_time();
        if deadline <= now {
            let expired_entry = {
                let mut guard = ALARM_LIST.lock();
                match guard.peek() {
                    Some(entry) if entry.deadline <= now => guard.pop(),
                    _ => None,
                }
            };

            if let (Some(handler), Some(entry)) = (EXPIRED_TASK_HANDLER.get(), expired_entry) {
                handler(entry.pid);
            }
        } else {
            listener!(EVENT_NEW_TIMER => listener);
            if ALARM_LIST
                .lock()
                .peek()
                .is_none_or(|entry| entry.deadline != deadline)
            {
                continue;
            }

            let _ = timeout_at(Some(deadline), listener).await;
        }
    }
}

/// Spawns the alarm task.
pub fn spawn_alarm_task() {
    ktask::spawn_raw(
        || block_on(alarm_task()),
        "alarm_task".to_owned(),
        kbuild_config::TASK_STACK_SIZE,
    );
}

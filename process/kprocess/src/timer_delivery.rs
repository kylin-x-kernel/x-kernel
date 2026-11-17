// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Thread/process timer runtime surface.
//!
//! This module owns the bridge between process-owned timer state and signal
//! delivery. Timer expiration polling and alarm-task registration converge
//! here; dequeue validation belongs to the originating process runtime.

use alloc::vec::Vec;

use klazy::Once;
use ksignal::SignalInfo;
use ktimer::{TimerDelivery, TimerSignal};

use crate::{
    AsThread, Pid, Thread, lookup,
    process_signals::{send_notification_to_process, send_notification_to_thread},
};

pub(crate) fn wake_requeued_timer(target: &ksignal::TimerSignalTarget, signo: ksignal::Signo) {
    if let Some(tid) = target.pending_wake_tid(signo)
        && let Ok(task) = lookup::task(tid)
        && let Some(thread) = task.try_as_thread()
        && target.matches_thread(thread.signal_manager())
    {
        ktask::signal_task(&task);
    }
}

/// Dispatches a single timer delivery to its process or thread target.
/// POSIX deliveries use their captured manager/slot, not the numeric envelope.
/// Legacy itimers retain numeric routing. No timer or signal lock may be held.
pub fn dispatch_timer_delivery(process_pid: Pid, delivery: TimerDelivery) {
    let (legacy_tid, signal) = match delivery {
        TimerDelivery::Process(signal) => (None, signal),
        TimerDelivery::Thread { tid, signal } => (Some(tid), signal),
    };
    match signal {
        TimerSignal::Posix {
            target,
            notification,
        } => {
            let outcome = target.send(notification);
            if let Some(tid) = outcome.wake_tid()
                && let Ok(task) = lookup::task(tid)
                && let Some(thread) = task.try_as_thread()
                && target.matches_thread(thread.signal_manager())
            {
                ktask::signal_task(&task);
            }
            drop(outcome);
        }
        TimerSignal::Legacy { signo } => {
            let notification = SignalInfo::new_kernel(signo).into();
            if let Some(tid) = legacy_tid {
                let _ = send_notification_to_thread(None, tid, Some(notification));
            } else {
                let _ = send_notification_to_process(process_pid, Some(notification));
            }
        }
    }
}

fn dispatch_timer_deliveries(process_pid: Pid, deliveries: Vec<TimerDelivery>) {
    for delivery in deliveries {
        dispatch_timer_delivery(process_pid, delivery);
    }
}

pub(crate) fn poll_timer(pid: Pid) {
    let Ok(process) = lookup::live_process(pid) else {
        return;
    };
    let Ok(deliveries) = process.poll_wall_clock_timers() else {
        return;
    };
    dispatch_timer_deliveries(pid, deliveries);
}

/// Polls CPU-driven timers for the given user thread's process.
///
/// A periodic tick requests this work; multiple ticks coalesce until user return.
///
/// # Panics
///
/// Panics if the process timer runtime is gone, which a live `&Thread` rules
/// out: the thread holds the runtime's strong `Arc`.
pub fn poll_cpu_timers_for(thread: &Thread) {
    if !thread.take_cpu_timer_work() || !thread.process().cpu_timers_armed() {
        return;
    }
    let process = thread.process().clone();
    let deliveries = process
        .poll_cpu_timers()
        .expect("a borrowed thread keeps its process timer runtime attached");
    dispatch_timer_deliveries(process.pid(), deliveries);
}

static TIMER_RUNTIME_INIT: Once<()> = Once::new();

/// Installs the timer-expiration bridge and starts the timer runtime once.
pub fn init_timer_runtime() {
    TIMER_RUNTIME_INIT.call_once(|| {
        ktimer::register_expired_task_handler(poll_timer);
        ktimer::spawn_alarm_task();
    });
}

/// Starts the timer runtime by spawning the alarm task once.
pub fn spawn_alarm_task() {
    init_timer_runtime();
}

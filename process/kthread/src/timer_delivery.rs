// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Thread/process timer runtime surface.
//!
//! This module owns the bridge between process-owned timer state and signal
//! delivery. Timer expiration polling, dequeue validation callbacks, and
//! alarm-task registration all converge here.

use alloc::vec::Vec;

use klazy::Once;
use kprocess::Pid;
use ksignal::{SignalDequeueAction, SignalInfo, Signo};
use ktimer::{TimerDelivery, TimerSignal};

use crate::{
    get_process_state,
    signal::{send_signal_to_process, send_signal_to_thread},
};

/// Dispatches a single timer delivery to its process or thread target.
pub fn dispatch_timer_delivery(process_pid: Pid, delivery: TimerDelivery) {
    match delivery {
        TimerDelivery::Process(signal) => {
            let _ = send_signal_to_process(process_pid, Some(build_timer_signal_info(signal)));
        }
        TimerDelivery::Thread { tid, signal } => {
            let _ = send_signal_to_thread(None, tid, Some(build_timer_signal_info(signal)));
        }
    }
}

fn dispatch_timer_deliveries(process_pid: Pid, deliveries: Vec<TimerDelivery>) {
    for delivery in deliveries {
        dispatch_timer_delivery(process_pid, delivery);
    }
}

/// Polls process-scoped realtime timers for a process.
pub(crate) fn poll_timer(pid: Pid) {
    let Ok(proc_state) = get_process_state(pid) else {
        return;
    };

    dispatch_timer_deliveries(pid, proc_state.timer_manager().lock().poll_wall_clock());
}

/// Polls CPU-driven timers for the current thread's process.
pub fn poll_cpu_timers() {
    let proc_state = crate::current_process_state();
    let (process_utime_ns, process_stime_ns) = proc_state.process_cpu_time_ns();
    let deliveries = proc_state
        .timer_manager()
        .lock()
        .poll_cpu_timers(process_utime_ns, process_stime_ns);
    dispatch_timer_deliveries(proc_state.proc.pid(), deliveries);
}

/// Updates timer bookkeeping after a pending timer signal is dequeued.
fn on_signal_dequeued(sig: &SignalInfo) -> SignalDequeueAction {
    let Some(timer_id) = sig.timer_id() else {
        return SignalDequeueAction::Deliver;
    };
    let Some(signal_seq) = sig.timer_signal_seq() else {
        return SignalDequeueAction::Drop;
    };
    let should_deliver = crate::current_thread()
        .process_state()
        .timer_manager()
        .lock()
        .on_timer_signal_dequeued(timer_id, signal_seq);
    if should_deliver {
        SignalDequeueAction::Deliver
    } else {
        SignalDequeueAction::Drop
    }
}

fn build_timer_signal_info(signal: TimerSignal) -> SignalInfo {
    match signal {
        TimerSignal::Legacy { signo } => SignalInfo::new_kernel(signo),
        TimerSignal::Posix {
            signo,
            timer_id,
            overrun,
            signal_seq,
            value,
        } => SignalInfo::new_timer(signo, timer_id, overrun, value, signal_seq),
    }
}

static TIMER_RUNTIME_INIT: Once<()> = Once::new();

/// Installs the timer-expiration bridge and spawns the alarm task once.
pub fn spawn_alarm_task() {
    TIMER_RUNTIME_INIT.call_once(|| {
        ktimer::register_expired_task_handler(poll_timer);
        // Only register for signals that timers can produce:
        // SIGALRM (ITIMER_REAL), SIGVTALRM (ITIMER_VIRTUAL),
        // SIGPROF (ITIMER_PROF), and the full RT range (POSIX timers).
        for signo in [Signo::SIGALRM, Signo::SIGVTALRM, Signo::SIGPROF] {
            ksignal::register_signal_observer(signo, on_signal_dequeued);
        }
        for raw in Signo::SIGRTMIN as u8..=Signo::SIGRT32 as u8 {
            if let Some(signo) = Signo::from_repr(raw) {
                ksignal::register_signal_observer(signo, on_signal_dequeued);
            }
        }
        ktimer::spawn_alarm_task();
    });
}

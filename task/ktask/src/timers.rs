// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Explicit periodic timer callbacks (independent of the scheduler tick).

use alloc::{sync::Arc, vec::Vec};
use core::{
    sync::atomic::{AtomicPtr, Ordering},
    time::Duration,
};

use khal::time::monotonic_time_nanos;
use kspin::NoPreemptIrqSave;
use ktime_types::SystemTime;

use crate::api::rearm_local_timer;

type TimerIrqNoteFn = fn();

static TIMER_IRQ_NOTE: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// Registers a hook invoked from every local timer IRQ (`on_timer_fire`).
///
/// The hook runs with IRQs disabled and must stay short and non-blocking.
/// Replacing the hook is last-writer-wins. Used by watchdog so hardlockup
/// heartbeats on any timer IRQ, not only the 4s sample callback.
pub fn register_timer_irq_note(note_fn: TimerIrqNoteFn) {
    TIMER_IRQ_NOTE.store(note_fn as *mut (), Ordering::Release);
}

pub(crate) fn run_timer_irq_note() {
    let ptr = TIMER_IRQ_NOTE.load(Ordering::Acquire);
    if ptr.is_null() {
        return;
    }
    // SAFETY: `register_timer_irq_note` stores only a `'static` `fn()`.
    let note_fn: TimerIrqNoteFn = unsafe { core::mem::transmute(ptr) };
    note_fn();
}

struct PeriodicCallback {
    period_ns: u64,
    next_deadline_ns: u64,
    callback: Arc<dyn Fn(SystemTime) + Send + Sync>,
}

percpu_static! {
    PERIODIC_CALLBACKS: Vec<PeriodicCallback> = Vec::new(),
}

fn duration_to_period_ns(period: Duration) -> u64 {
    u64::try_from(period.as_nanos()).unwrap_or(u64::MAX)
}

fn deadline_after(completed_ns: u64, period_ns: u64) -> u64 {
    completed_ns.saturating_add(period_ns)
}

/// Registers a callback that fires every `period` on the current CPU.
///
/// Unlike the old tick-driven hook, this is an explicit periodic source that
/// participates in local timer arbitration on its own deadline.
///
/// Callbacks run with local IRQs disabled and must remain short and
/// non-blocking. The next firing is one full period after callback completion,
/// so overruns neither catch up nor immediately retrigger. Registering another
/// periodic callback from a callback is supported.
///
/// Periods beyond the `u64` nanosecond horizon saturate to that horizon.
///
/// # Panics
///
/// Panics if `period` is zero.
pub fn register_timer_callback<F>(period: Duration, callback: F)
where
    F: Fn(SystemTime) + Send + Sync + 'static,
{
    assert!(
        !period.is_zero(),
        "periodic timer callback period must be non-zero"
    );
    // Durations beyond the monotonic u64-nanosecond horizon are effectively
    // "never" timers. Saturating is safer than truncating to a short period.
    let period_ns = duration_to_period_ns(period);
    let now = monotonic_time_nanos();
    let _g = NoPreemptIrqSave::new();
    // SAFETY: the per-CPU callback vector is accessed on the current CPU while
    // preemption and interrupts are masked by `NoPreemptIrqSave`.
    unsafe {
        PERIODIC_CALLBACKS
            .current_ref_mut_raw()
            .push(PeriodicCallback {
                period_ns,
                next_deadline_ns: deadline_after(now, period_ns),
                callback: Arc::new(callback),
            });
    };
    let cpu_id = khal::percpu::this_cpu_id();
    rearm_local_timer(crate::future::earliest_deadline(cpu_id), None);
}

/// Runs due periodic callbacks and returns the earliest pending deadline.
///
/// The second tuple element is `true` when at least one callback was due at
/// entry (used by `sched_stat` timer-IRQ classification).
pub(crate) fn run_due_and_earliest(now_ns: u64) -> (Option<u64>, bool) {
    let mut any_due = false;
    // Capture the initial length. Registration from a callback may append and
    // reallocate the vector, but existing indices remain stable because there
    // is no callback-removal API.
    // SAFETY: timer IRQ context has local IRQs disabled.
    let callback_count = unsafe { PERIODIC_CALLBACKS.current_ref_raw().len() };
    for index in 0..callback_count {
        let check_now = if index == 0 {
            now_ns
        } else {
            monotonic_time_nanos()
        };
        // Clone the callback and release the vector borrow before invocation.
        // This makes registering another callback from inside a callback safe.
        // Mark the entry inactive so an intermediate rearm cannot immediately
        // retrigger an overrun callback.
        // SAFETY: timer IRQ context exclusively accesses this CPU's vector.
        let due = unsafe {
            let callbacks = PERIODIC_CALLBACKS.current_ref_mut_raw();
            let cb = &mut callbacks[index];
            if check_now >= cb.next_deadline_ns {
                cb.next_deadline_ns = u64::MAX;
                Some((cb.period_ns, cb.callback.clone()))
            } else {
                None
            }
        };

        if let Some((period_ns, callback)) = due {
            any_due = true;
            callback(ktime::realtime());
            // Rearm from completion time. A callback that overruns its period
            // therefore fires once, then waits a full period instead of
            // causing a catch-up or immediate-IRQ loop.
            let completed_ns = monotonic_time_nanos();
            // SAFETY: callback registration only appends, so this entry's
            // index is still valid; IRQs remain disabled.
            unsafe {
                PERIODIC_CALLBACKS.current_ref_mut_raw()[index].next_deadline_ns =
                    deadline_after(completed_ns, period_ns);
            }
        }
    }
    (earliest_deadline(), any_due)
}

/// Earliest pending periodic-callback deadline without running callbacks.
pub(crate) fn earliest_deadline() -> Option<u64> {
    // SAFETY: caller must hold IRQs off on this CPU (rearm / timer paths).
    let callbacks = unsafe { PERIODIC_CALLBACKS.current_ref_raw() };
    // Skip in-flight entries (`u64::MAX`). Arming that sentinel programs the
    // 32-bit TVAL clamp (~tens of seconds) and looks like a hardlockup.
    callbacks
        .iter()
        .map(|cb| cb.next_deadline_ns)
        .filter(|&d| d != u64::MAX)
        .min()
}

#[cfg(unittest)]
mod tests {
    use core::time::Duration;

    use unittest::{assert_eq, def_test};

    use super::{deadline_after, duration_to_period_ns};

    #[def_test]
    fn periodic_duration_conversion_saturates() {
        assert_eq!(duration_to_period_ns(Duration::from_nanos(1)), 1);
        assert_eq!(
            duration_to_period_ns(Duration::from_secs(u64::MAX)),
            u64::MAX
        );
    }

    #[def_test]
    fn periodic_rearm_starts_after_callback_completion() {
        assert_eq!(deadline_after(1_000, 200), 1_200);
        assert_eq!(deadline_after(u64::MAX - 10, 20), u64::MAX);
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Owned prepared timers and legacy periodic callbacks.

use alloc::{sync::Arc, vec::Vec};
use core::{
    sync::atomic::{AtomicPtr, Ordering},
    time::Duration,
};

use khal::time::monotonic_time_nanos;
use kspin::NoPreemptIrqSave;
use ktime_types::{MonotonicInstant, SystemTime};
use ktimer_core::TimerSet;
pub use ktimer_core::{CancelResult, Expiration, TimerControl, TimerHandler};

use crate::api::rearm_local_timer;

/// Static CPU queues used by the hardware timer service.
/// Owned timers register here and refresh the actual owner's hardware.
pub(crate) static PREPARED_TIMERS: TimerSet<{ kbuild_config::NR_CPUS }> = TimerSet::new();

/// An owned one-shot timer served by the kernel's timer IRQ.
///
/// Preparation allocates once; start, cancellation and callback rearm reuse
/// that storage. Drop synchronously drains delivery before releasing it.
/// Cancellation and Drop must not run from this timer's callback or while
/// holding a lock needed by it, or otherwise prevent its CPU from progressing.
/// Repetition is explicit through [`TimerControl::rearm`], not automatic.
///
/// # Examples
///
/// ```no_run
/// use ktask::{Expiration, Timer, TimerControl};
/// use ktime_types::TimeSpan;
///
/// let timer = Timer::new(|_: TimerControl<'_>, _: Expiration| {});
/// // SAFETY: run after local scheduler/timer initialization; the empty handler
/// // is IRQ-compatible, and the owned handle remains live until cancellation.
/// unsafe { timer.start(khal::time::monotonic_time() + TimeSpan::from_millis(1)) }.unwrap();
/// timer.cancel_sync();
/// ```
pub struct Timer<H: TimerHandler + 'static> {
    inner: ktimer_core::Timer<H, { kbuild_config::NR_CPUS }>,
}

impl<H: TimerHandler + 'static> Timer<H> {
    /// Allocates inactive, stable node and handler storage.
    /// Allocation failure follows the kernel allocator's failure policy.
    ///
    /// # Panics
    /// Panics if the core's newly allocated storage is not uniquely owned,
    /// an internal invariant unreachable from valid caller input.
    pub fn new(handler: H) -> Self {
        Self {
            inner: ktimer_core::Timer::new(&PREPARED_TIMERS, handler),
        }
    }

    /// Borrows the handler stored beside the timer node without cloning or allocating.
    /// The reference cannot outlive this timer. Handler state may be accessed
    /// concurrently by IRQ delivery, so mutation requires interior synchronization.
    /// Release any handler lock needed by delivery before cancelling or dropping
    /// the timer; synchronous draining must let that delivery finish.
    pub fn handler(&self) -> &H {
        self.inner.handler()
    }

    /// Starts or replaces one deadline without allocation or inline delivery.
    ///
    /// Idle and pending timers select the current CPU. A running callback,
    /// including cleanup, retains its owner. The actual owner is refreshed
    /// locally or notified by fixed IPI after the core releases its locks.
    /// Past deadlines are accepted for subsequent IRQ delivery.
    ///
    /// # Errors
    /// Returns `InvalidInput` for deadlines beyond `u64::MAX` nanoseconds or
    /// an out-of-range CPU, without changing the previous setting.
    ///
    /// # Panics
    /// Panics if an accepted owner's hardware refresh violates the initialized
    /// CPU invariant (including failed remote notification).
    ///
    /// # Safety
    /// Call only after the current CPU's scheduler and timer IRQ service are
    /// ready. The handler must not sleep, unwind, enable IRQs, reenter timer
    /// dispatch or synchronously cancel itself. These obligations apply to
    /// every delivery, including callback rearm and captured-value cleanup.
    pub unsafe fn start(&self, deadline: MonotonicInstant) -> kerrno::KResult<()> {
        let _guard = NoPreemptIrqSave::new();
        let cpu = khal::percpu::this_cpu_id();
        // SAFETY: the guard pins the initialized CPU with IRQs masked; owned
        // core storage remains stable. The caller supplies callback constraints
        // and refresh below targets the actual owner after releasing core locks.
        let (owner, _) = unsafe { self.inner.start_on(cpu, deadline) }?;
        refresh_owner(owner, cpu);
        Ok(())
    }

    /// Accepts a wait only when its deadline is future at the locked check.
    ///
    /// # Safety
    /// Same initialized service and handler obligations as [`Self::start`].
    pub(crate) unsafe fn start_if_future(
        &self,
        deadline: MonotonicInstant,
    ) -> kerrno::KResult<bool> {
        let _guard = NoPreemptIrqSave::new();
        let cpu = khal::percpu::this_cpu_id();
        // SAFETY: the guard pins this initialized CPU; the owned core retains
        // node/handler storage. HAL clock reads are nonblocking and monotonic.
        // The caller supplies handler constraints; notification follows below.
        let accepted = unsafe {
            self.inner
                .start_on_if_future(cpu, deadline, monotonic_time_nanos)
        }?;
        if let Some((owner, _)) = accepted {
            refresh_owner(owner, cpu);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Attempts cancellation without waiting or allocating.
    ///
    /// `Running` leaves an in-flight delivery and its pending successor intact.
    /// May be called from the callback. Concurrent starts may reactivate the
    /// timer; this is not a lifetime barrier. A stale hardware IRQ may remain.
    pub fn try_cancel(&self) -> CancelResult {
        self.inner.try_cancel()
    }

    /// Waits for delivery cleanup and removes any pending successor.
    ///
    /// Returns whether pending membership was removed. Does not allocate or
    /// sleep. Exclude concurrent starts for a durable stop; never call from the
    /// callback itself or while preventing its progress. A stale IRQ may remain.
    pub fn cancel_sync(&self) -> bool {
        self.inner.cancel_sync()
    }
}

// Caller is CPU-pinned with IRQs masked and holds no timer locks.
fn refresh_owner(owner: kcpu_id_map::LogicalCpuId, cpu: kcpu_id_map::LogicalCpuId) {
    if owner == cpu {
        crate::future::reprogram_local_timer();
    } else {
        #[cfg(feature = "smp")]
        kipi::notify_timer_reprogram(owner).expect("active timer owner is an initialized CPU");
        #[cfg(not(feature = "smp"))]
        unreachable!("a uniprocessor timer cannot have a remote owner");
    }
}

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

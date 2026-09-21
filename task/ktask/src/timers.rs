// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Owned prepared timers and the timer IRQ heartbeat hook.

use core::sync::atomic::{AtomicPtr, Ordering};

use khal::time::monotonic_time_nanos;
use kspin::NoPreemptIrqSave;
use ktime_types::MonotonicInstant;
use ktimer_core::TimerSet;
pub use ktimer_core::{CancelResult, Expiration, TimerControl, TimerHandler};

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
    /// Call after platform per-CPU initialization. The current CPU supplies
    /// only an initial route hint; a later start still selects its own CPU.
    ///
    /// # Panics
    /// Panics if the core's newly allocated storage is not uniquely owned,
    /// or the platform reports an out-of-range CPU, internal invariants
    /// unreachable after valid platform initialization.
    pub fn new(handler: H) -> Self {
        // Reading the CPU ID is protected by the platform accessor. Migration
        // after this snapshot is harmless: start revalidates and moves the route.
        let initial_cpu = khal::percpu::this_cpu_id();
        Self {
            inner: ktimer_core::Timer::new_on(&PREPARED_TIMERS, initial_cpu, handler)
                .expect("current CPU must have a prepared timer slot"),
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
        crate::api::rearm_local_timer();
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

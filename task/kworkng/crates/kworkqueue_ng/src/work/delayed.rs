// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Externally retained Work with one prepared timer and a non-owning callback binding.

use alloc::sync::Arc;
use core::{
    pin::Pin,
    ptr::NonNull,
    sync::atomic::{AtomicPtr, Ordering},
};

use kcpu_id_map::LogicalCpuId;
use ktask::{Expiration, Timer, TimerControl, TimerHandler};
#[cfg(unittest)]
use ktime_types::MonotonicInstant;
use ktime_types::TimeSpan;

use super::{
    Work,
    disable::DisableGuard,
    wait::{CleanupWait, WaitError},
};
use crate::queue::{Queue, SubmitError};

/// Failure before accepting a delayed-work occurrence, including zero delay.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArmError {
    /// Queue admission or ownership validation rejected the operation.
    Admission(SubmitError),
    /// The deadline or timer operation could not be represented or accepted.
    Timer(kerrno::KError),
}

/// Pre-capture failure of a delayed flush, without canceling accepted work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FlushError {
    /// Completion-wait preparation failed before capture.
    Wait(WaitError),
    /// Moving delayed work into ordinary queueing was rejected.
    Transfer(SubmitError),
}

/// Caller-owned delayed work; the timer borrows Work and never owns its captures.
///
/// Pin before first publication. Stop producers and successfully cancel_sync
/// before destruction, including logically idle objects: physical delivery may
/// still borrow Work. Drop is not automatic ordinary-work drain. No inner Work
/// or handler mutation is exposed to callers.
pub struct DelayedWork<H> {
    // Timer storage is dropped first; this alone does not drain ordinary work.
    timer: Timer<DelayedTimerHandler>,
    work: Work<Arc<Queue>, H>,
}

impl<H: Fn() + Send + Sync> DelayedWork<H> {
    /// Prepares one timer allocation and inline Work, without publishing an address.
    /// Requires initialized per-CPU state; preparation may allocate in task context.
    ///
    /// # Panics
    /// Propagates timer preparation invariant failures; OOM follows kernel policy.
    pub fn new(callback: H) -> Self {
        Self {
            timer: Timer::new(DelayedTimerHandler {
                target: AtomicPtr::new(core::ptr::null_mut()),
                deliver: Self::deliver,
                #[cfg(unittest)]
                delivery_tail: kspin::SpinNoIrq::new(None),
            }),
            work: Work::with_callback(callback),
        }
    }

    /// Accepts one occurrence after an elapsed delay on an explicit execution CPU.
    ///
    /// Zero delay uses ordinary submission immediately, without a timer tick or
    /// inline callback. Positive delay is measured from a monotonic observation
    /// in this call; late scheduling is allowed, early eligibility is not.
    /// This is fresh admission, not modification of an existing pending occurrence.
    ///
    /// # Safety
    /// Same stable external storage, initialized service and queue-context
    /// obligations as arm_on. Stop/join producers and successfully cancel_sync
    /// before releasing storage, including after zero-delay submission.
    ///
    /// # Errors
    /// Unrepresentable deadlines and admission failures leave existing work
    /// unchanged. Concurrent failure reasons have no promised priority.
    ///
    /// # Panics
    /// Propagates timer or ordinary submission invariant failures.
    #[cfg(unittest)]
    pub(crate) unsafe fn queue_after_on(
        self: Pin<&Self>,
        queue: &Arc<Queue>,
        cpu: LogicalCpuId,
        delay: TimeSpan,
    ) -> Result<(), ArmError> {
        // SAFETY: the same pinned object, initialized service and retention
        // contract apply; explicit selection does not invoke the CPU sampler.
        unsafe { self.queue_after(queue, Some(cpu), delay, khal::percpu::this_cpu_id) }.map(|_| ())
    }

    /// Accepts a relative delay with explicit or Queue-policy CPU selection.
    /// Reuses unfinished ownership; only fresh successful automatic RR admission
    /// advances the Queue cursor. Zero delay uses the identical ordinary router.
    /// The current_cpu provider has the same sampling contract as Queue::submit.
    ///
    /// # Safety
    /// Same pinning, external retention, callback and initialized-service contract
    /// as arm_on. current_cpu must observe the actual logical CPU without allocation.
    ///
    /// # Errors
    /// Rejection preserves existing placement and routing progress.
    ///
    /// # Panics
    /// Propagates timer or ordinary submission invariant failures.
    pub(crate) unsafe fn queue_after(
        self: Pin<&Self>,
        queue: &Arc<Queue>,
        target: Option<LogicalCpuId>,
        delay: TimeSpan,
        current_cpu: impl Fn() -> LogicalCpuId,
    ) -> Result<LogicalCpuId, ArmError> {
        if delay.is_zero() {
            // SAFETY: caller retains this pinned Work and captures through all
            // ordinary accesses; Queue applies the existing routing contract.
            return unsafe { queue.submit(&self.work, target, current_cpu) }
                .map_err(ArmError::Admission);
        }
        let deadline = khal::time::monotonic_time()
            .checked_add(delay)
            .ok_or(ArmError::Timer(kerrno::KError::InvalidInput))?;
        self.timer.handler().bind(NonNull::from(&self.work).cast());
        // SAFETY: fixed binding points to this pinned Work; caller retains it
        // through timer and ordinary drain. Queue owns routing and its revalidation.
        unsafe { queue.arm_delayed(&self.work, &self.timer, target, deadline, current_cpu) }
    }

    /// Arms a fresh timer-side occurrence for an explicit allowed execution CPU.
    ///
    /// This internal absolute-deadline entry is not retime, relative-delay/zero-delay
    /// submission or automatic CPU routing. Existing pending work rejects unchanged;
    /// a running predecessor permits one successor on its original partition.
    /// Timer IRQ may run on another CPU; ordinary callback execution uses cpu.
    ///
    /// # Safety
    /// Keep pinned storage and captures live/shared through all calls, physical
    /// timer delivery and ordinary cleanup. Stop/join producers and successfully
    /// cancel_sync before releasing storage. Current CPU scheduler/timer service
    /// must be initialized. Publication and eventual callback context must obey
    /// the selected queue's normal/BH execution contract.
    ///
    /// # Errors
    /// Admission or timer validation failures leave logical placement unchanged.
    /// A failed first call may establish the permanent non-owning address binding.
    ///
    /// # Panics
    /// Panics on a changed bound address or violated initialized timer invariants.
    #[cfg(unittest)]
    pub(crate) unsafe fn arm_on(
        self: Pin<&Self>,
        queue: &Arc<Queue>,
        cpu: LogicalCpuId,
        deadline: MonotonicInstant,
    ) -> Result<(), ArmError> {
        self.timer.handler().bind(NonNull::from(&self.work).cast());
        // SAFETY: Pin and caller retain this exact Work through timer/ordinary drain.
        // Queue supplies its owned partition; caller supplies initialized service.
        unsafe {
            queue.arm_delayed(
                &self.work,
                &self.timer,
                Some(cpu),
                deadline,
                khal::percpu::this_cpu_id,
            )
        }
        .map(|_| ())
    }

    /// Changes an existing pending occurrence to an absolute timer deadline.
    /// Returns false when no pending target remains, including if claim wins.
    /// This internal primitive is not arrange-or-modify: it creates no successor,
    /// changes no Queue/CPU owner and does not implement the zero-delay fast path.
    ///
    /// # Safety
    /// Same pinned storage, capture retention and initialized-service obligations
    /// as arm_on. Join producers and cancel_sync before releasing external storage.
    ///
    /// # Errors
    /// Rejection preserves the old deadline, FIFO, allowance and completion scope.
    ///
    /// # Panics
    /// Propagates timer and queue invariant failures or observer wakeup panics.
    #[cfg(unittest)]
    pub(crate) unsafe fn retime_pending_at(
        self: Pin<&Self>,
        deadline: MonotonicInstant,
    ) -> Result<bool, ArmError> {
        self.timer.handler().bind(NonNull::from(&self.work).cast());
        loop {
            let owner = self.work.state.lock().pending_partition();
            let Some(owner) = owner else { return Ok(false) };
            // SAFETY: same fixed binding and pinned Work retained by the caller;
            // snapshot guard ends before acquiring partition -> Work protection.
            if unsafe { owner.retime_pending(&self.work, &self.timer, Some(deadline)) }? {
                return Ok(true);
            }
        }
    }

    /// Adjusts pending delay, or accepts one new occurrence if none remains.
    /// Zero queues immediately without inline execution or a timer tick. Existing
    /// ordinary queueing keeps its position when adjusted to zero. Unfinished
    /// Queue/CPU ownership cannot change; running work itself is never modified.
    /// The relative deadline is sampled once and retained across admission races.
    ///
    /// # Safety
    /// Same external pinning, capture retention and initialized-service contract
    /// as queue_after; current_cpu must sample the actual CPU without allocation.
    /// Stop producers and cancel_sync before freeing storage.
    ///
    /// # Errors
    /// Failure preserves the existing arrangement, except independent concurrent
    /// operations. No fresh occurrence is accepted by a failed attempt.
    ///
    /// # Panics
    /// Propagates timer, queue and observer notification invariant failures.
    pub(crate) unsafe fn modify_after(
        self: Pin<&Self>,
        queue: &Arc<Queue>,
        target: Option<LogicalCpuId>,
        delay: TimeSpan,
        current_cpu: impl Fn() -> LogicalCpuId,
    ) -> Result<LogicalCpuId, ArmError> {
        let deadline = if delay.is_zero() {
            None
        } else {
            Some(
                khal::time::monotonic_time()
                    .checked_add(delay)
                    .ok_or(ArmError::Timer(kerrno::KError::InvalidInput))?,
            )
        };
        self.timer.handler().bind(NonNull::from(&self.work).cast());
        // SAFETY: this fixed binding borrows the same externally pinned Work.
        // Caller retains storage through both timer and ordinary drains.
        unsafe { queue.modify_delayed(&self.work, &self.timer, target, deadline, current_cpu) }
    }

    /// Revokes unclaimed work without waiting for running work or timer delivery.
    /// Timer removal and logical revocation share Work protection, so an old
    /// cancel cannot remove a concurrent producer's new timer. Running physical
    /// delivery may survive and must revalidate logical placement. False means no
    /// logical pending work was revoked, not that physical activity is drained.
    /// IRQ-compatible, allocation-free; does not disable future submissions.
    /// External owners must still join producers and cancel_sync before release.
    ///
    /// # Panics
    /// Propagates queue/timer invariant and unlocked cleanup/notification panics.
    pub fn cancel_pending(&self) -> bool {
        self.work.cancel_pending_with(|| {
            // A Running result is harmless: logical revocation is authoritative.
            // Final synchronized drain, not this attempt, authorizes storage release.
            let _ = self.timer.try_cancel();
        })
    }

    /// Holds one independent disable and revokes pending work without waiting.
    /// The retained ordinary guard blocks both queueing and delay modification.
    /// Running work/delivery may continue; dropping the guard permits new work
    /// without restoring canceled work. IRQ-compatible and allocation-free.
    /// This is not a storage-release barrier; use disable_sync or cancel_sync.
    ///
    /// # Panics
    /// Propagates disable-depth overflow and cleanup/notification invariant failures.
    pub fn disable(&self) -> (DisableGuard<'_, Arc<Queue>, H>, bool) {
        let (guard, revoked) = self.work.disable();
        // The held disable prevents any start while ordinary cancellation and
        // this physical attempt run. No management lock spans cleanup above.
        // A Running result is harmless; final synchronized cancellation drains it.
        let _ = self.timer.try_cancel();
        (guard, revoked)
    }

    /// Holds one independent disable through ordinary and physical timer drain.
    /// Success retains the disable until the guard is dropped. It does not join
    /// concurrent API invocations; stop/join producers before releasing storage.
    /// Sleepable context only; do not block progress or wait from this callback.
    ///
    /// # Errors
    /// Context and self-wait errors precede all effects.
    ///
    /// # Panics
    /// Propagates fatal preparation, disable-depth and cleanup invariant failures.
    pub fn disable_sync(&self) -> Result<(DisableGuard<'_, Arc<Queue>, H>, bool), WaitError> {
        let (guard, revoked) = self.work.disable_sync()?;
        self.timer.cancel_sync();
        Ok((guard, revoked))
    }

    /// Revokes pending work and drains both ordinary execution and physical delivery.
    /// Keeps its temporary disable through the timer drain, including for idle Work.
    /// Caller must exclude producers before using success as permission to free storage.
    /// Do not prevent timer/callback progress or invoke from the same business callback.
    ///
    /// # Errors
    /// Context and optional self-wait errors precede all effects.
    ///
    /// # Panics
    /// Propagates disable-depth overflow and fatal preparation/cleanup failures.
    pub fn cancel_sync(&self) -> Result<bool, WaitError> {
        let (guard, revoked) = self.disable_sync()?;
        drop(guard);
        Ok(revoked)
    }

    /// Expedites delayed work and waits for the queueing/execution captured here.
    /// Concurrent retiming may withdraw that queueing and end this wait without
    /// executing its callback. Later timers are not canceled or followed. Success
    /// does not prove idleness or permit storage release; join producers and use
    /// cancel_sync for teardown. Sleepable context only, with callback progress.
    ///
    /// # Errors
    /// Context, self-wait and transfer errors precede scope capture
    /// and leave the accepted arrangement unchanged by this call.
    ///
    /// # Panics
    /// Propagates broken timer, queue, accounting or notification invariants.
    pub fn flush(&self) -> Result<(), FlushError> {
        let task_wait = ktask::completion::CompletionWait::prepare()
            .map_err(|_| FlushError::Wait(WaitError::InvalidContext))?;
        #[cfg(feature = "self-wait-check")]
        self.work
            .reject_callback_self_wait()
            .map_err(FlushError::Wait)?;
        // Work has at most one pending and one running slot. Prepare once so
        // physical-delivery retries do not allocate more receipts.
        let mut wait = CleanupWait::prepare(2);
        loop {
            let owner = self.work.state.lock().owner_partition();
            let captured = if let Some(owner) = owner {
                // SAFETY: prior publication requires stable external storage
                // through this call and both drains; the timer belongs to this
                // Work. Preflight and maximum receipt preparation finished above.
                unsafe { owner.capture_and_expedite(&self.work, &self.timer, &mut wait.prepared) }
                    .map_err(FlushError::Transfer)?
            } else {
                let state = self.work.state.lock();
                // Recheck under Work protection before stopping even an idle
                // timer: a producer may have published after the owner snapshot.
                state.owner_matches(None) && self.timer.try_cancel() != ktask::CancelResult::Running
            };
            if captured {
                break;
            }
            // Neither management locks nor IRQ masking span the retry. Yield
            // also lets same-CPU producers complete before ownership revalidation.
            ktask::yield_now();
        }
        wait.discard_unused();
        wait.wait(&task_wait);
        Ok(())
    }

    /// Transfers a due occurrence using its current owner and placement.
    ///
    /// # Safety
    /// target must be this constructor's Work<Arc<Queue>, H>, live, pinned and
    /// shared until delivery returns. Binding never points at the outer shell.
    unsafe fn deliver(target: NonNull<()>, now_ns: u64) {
        // SAFETY: new binds this concrete delivery entry; submission publishes exactly
        // its Work field pointer. External ownership and synchronous drain keep
        // storage and captures live through this local borrow and any queued work.
        let work = unsafe { target.cast::<Work<Arc<Queue>, H>>().as_ref() };
        let partition = work.state.lock().owner_partition();
        let Some(partition) = partition else { return };
        // SAFETY: same caller-owned storage; the snapshot guard ended before
        // partition acquisition. queue_due revalidates ownership and placement.
        match unsafe { partition.queue_due(work, now_ns) } {
            Ok(_) | Err(SubmitError::Disabled | SubmitError::Closed) => {}
            // Accepted work cannot silently disappear on an impossible accounting
            // overflow. Other errors are not produced by the transfer transaction.
            #[expect(
                clippy::panic,
                reason = "CG-021-no-panic-on-reachable-failure: The transfer transaction fails \
                          only as Disabled or Closed; any other error is an impossible accounting \
                          overflow, and accepted work must not silently disappear"
            )]
            Err(error) => panic!("delayed transfer invariant failed: {error:?}"),
        }
    }

    #[cfg(unittest)]
    pub(crate) fn work_for_test(&self) -> &Work<Arc<Queue>, H> {
        &self.work
    }
}

/// Erased borrowing adapter, not another owner or copy of the Work state.
/// The fixed entry keeps non-'static callback captures out of Timer's owned type.
struct DelayedTimerHandler {
    target: AtomicPtr<()>,
    deliver: unsafe fn(NonNull<()>, u64),
    #[cfg(unittest)]
    delivery_tail: kspin::SpinNoIrq<Option<Arc<core::sync::atomic::AtomicUsize>>>,
}

impl DelayedTimerHandler {
    fn bind(&self, target: NonNull<()>) {
        let old = self.target.load(Ordering::Acquire);
        if !old.is_null() {
            assert_eq!(old, target.as_ptr(), "delayed Work moved after binding");
            return;
        }
        // Publication changes only this independent immutable address. Competing
        // producers can bind the same pinned object, never a replacement target.
        if let Err(old) = self.target.compare_exchange(
            core::ptr::null_mut(),
            target.as_ptr(),
            Ordering::Release,
            Ordering::Acquire,
        ) {
            assert_eq!(old, target.as_ptr(), "delayed Work binding changed");
        }
    }
}

impl TimerHandler for DelayedTimerHandler {
    fn on_expiration(&self, _: TimerControl<'_>, _: Expiration) {
        let target = NonNull::new(self.target.load(Ordering::Acquire))
            .expect("timer starts only after Work binding");
        // SAFETY: submission publishes only this fixed target/entry pair. Its unsafe
        // caller retains Work through timer drain; no timer lock spans this call.
        unsafe { (self.deliver)(target, khal::time::monotonic_time_nanos()) };
        #[cfg(unittest)]
        {
            let gate = self.delivery_tail.lock().clone();
            if let Some(gate) = gate {
                // Per-object test handshake holds the actual IRQ delivery tail,
                // after Work publication but before timer-core clears Running.
                gate.store(1, Ordering::Release);
                while gate.load(Ordering::Acquire) == 1 {
                    core::hint::spin_loop();
                }
            }
        }
    }
}

unittest::path_mod_test!(
    delayed_stop_unittest,
    "../../ut-tests/work_delayed_stop_unittest.rs"
);

unittest::path_mod_test!(
    delayed_flush_unittest,
    "../../ut-tests/work_delayed_flush_unittest.rs"
);

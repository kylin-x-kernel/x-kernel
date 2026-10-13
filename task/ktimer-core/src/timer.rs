// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Owning shell: drain borrowed timer accesses before releasing callback storage.

use alloc::sync::Arc;

use kcpu_id_map::LogicalCpuId;
use kerrno::KResult;
use ktime_types::MonotonicInstant;

use crate::{TimerHandler, TimerSet, deadline::Deadline, index::TimerNode};

struct TimerData<H> {
    node: TimerNode,
    handler: H,
}

/// Outcome of one non-waiting cancellation attempt, not a durable stop guarantee.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use]
pub enum CancelResult {
    /// No pending membership or in-flight delivery was present.
    Inactive,
    /// Pending membership was removed, with no in-flight delivery.
    Removed,
    /// Delivery is in flight; any pending successor is unchanged.
    Running,
}

/// Owns stable node and handler storage in one allocation prepared before use.
///
/// The shell may move; its private allocation cannot. Drop synchronously cancels
/// before releasing that allocation. Forgetting the shell retains the allocation
/// too, so it cannot invalidate a node borrowed by the tree. No Clone, mutable
/// handler access or Arc extraction is exposed.
///
/// Synchronous cancellation/Drop must not run in this timer's callback or while holding a
/// lock needed by it; the servicing CPU must be able to finish. Otherwise the
/// synchronous wait can deadlock. The handler must be hard-IRQ compatible.
/// See the [crate examples](crate#examples) for preparation, service and teardown.
pub struct Timer<H: TimerHandler + 'static, const N: usize> {
    timers: &'static TimerSet<N>,
    data: Arc<TimerData<H>>,
}

impl<H: TimerHandler + 'static, const N: usize> Timer<H, N> {
    /// Allocates inactive storage bound permanently to this timer collection.
    /// This preparation may allocate; subsequent start/cancel operations do not.
    /// Allocation failure follows the configured allocator's failure policy;
    /// this constructor does not return a recoverable allocation error.
    /// Initially routes to CPU0; use [`Self::new_on`] to choose another initial
    /// route. This is not affinity: start_on still selects the execution CPU.
    ///
    /// # Panics
    /// Panics if the newly created private Arc is not uniquely owned; this
    /// internal invariant failure is unreachable from valid caller input.
    pub fn new(timers: &'static TimerSet<N>, handler: H) -> Self {
        Self::prepare(timers, LogicalCpuId::new(0), handler)
    }

    /// Allocates inactive storage with a validated initial CPU lock route.
    /// Does not enqueue, program hardware or constrain later placement.
    /// Choosing the likely first-start CPU avoids an initial route migration.
    /// No CPU execution or IRQ readiness is required until the timer is started.
    /// Allocation failure follows the configured allocator's failure policy.
    ///
    /// # Errors
    /// Returns InvalidInput before allocating timer storage if initial_cpu is
    /// outside this collection. The supplied handler is dropped on rejection.
    ///
    /// # Panics
    /// Panics if the newly allocated private Arc is not uniquely owned, an
    /// internal invariant failure unreachable from valid caller input.
    ///
    /// # Examples
    /// ```
    /// use ktimer_core::{CancelResult, Expiration, Timer, TimerControl, TimerSet};
    /// static TIMERS: TimerSet<2> = TimerSet::new();
    /// let timer = Timer::new_on(&TIMERS, 1.into(), |_: TimerControl<'_>, _: Expiration| {}).unwrap();
    /// assert_eq!(timer.try_cancel(), CancelResult::Inactive);
    /// ```
    pub fn new_on(
        timers: &'static TimerSet<N>,
        initial_cpu: LogicalCpuId,
        handler: H,
    ) -> KResult<Self> {
        if initial_cpu.as_usize() >= N {
            return Err(kerrno::KError::InvalidInput);
        }
        Ok(Self::prepare(timers, initial_cpu, handler))
    }

    // Both constructors establish an in-range route; TimerSet rejects N=0.
    fn prepare(timers: &'static TimerSet<N>, initial_cpu: LogicalCpuId, handler: H) -> Self {
        let deadline = Deadline::ZERO;
        let mut data = Arc::new(TimerData {
            node: TimerNode::new(deadline),
            handler,
        });
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: This Arc was just allocated and no \
                      strong or weak alias has been published"
        )]
        let entry = Arc::get_mut(&mut data).expect("new timer storage is exclusively owned");
        // SAFETY: the unique, unpublished allocation is already at its final
        // address. Only the disjoint node field is replaced. No mutable access
        // to handler or the allocation is exposed after this binding. Drop drains
        // all borrowed node/handler accesses before releasing the private Arc.
        // The validated initial route is installed before publication, not by
        // bypassing the lock protocol for a previously published node.
        entry.node = unsafe { TimerNode::with_handler_on(deadline, &entry.handler, initial_cpu) };
        Self { timers, data }
    }

    /// Borrows the owned handler without cloning or exposing its storage owner.
    ///
    /// The borrow cannot outlive this timer. Delivery may access the handler
    /// concurrently, so shared mutable state must use the handler's own
    /// synchronization. Do not cancel or drop while holding a handler lock
    /// needed by an in-flight callback.
    pub fn handler(&self) -> &H {
        // SAFETY: new binds this private node to exactly H in the same retained
        // allocation. Use the bound pointer, as dispatch does, so writes through
        // H's interior mutability cannot invalidate the cached callback pointer.
        // No mutable handler access or ownership extraction is exposed.
        unsafe { self.data.node.handler::<H>() }
    }

    /// Starts or replaces a setting without allocating or invoking the handler.
    /// Returns the actual owner and whether pending membership was newly added.
    /// Idle and pending timers select cpu. In-flight delivery, including cleanup,
    /// retains its owner even when a successor is pending; no migration is deferred.
    /// Replacing pending membership reports false, including across CPUs.
    /// Deadlines are absolute monotonic times in `0..=u64::MAX` nanoseconds;
    /// past deadlines are accepted for later dispatch, never invoked inline.
    /// Each setting is one-shot; a callback must explicitly rearm to repeat.
    /// The maximum deadline is a far-future horizon: dispatch clocks must stay
    /// below it, so delivery at or beyond that horizon is not guaranteed.
    ///
    /// # Errors
    /// Returns [`kerrno::KError::InvalidInput`] if the deadline exceeds
    /// `u64::MAX` nanoseconds or `cpu` is outside this set.
    /// The previous setting remains unchanged.
    ///
    /// # Examples
    /// See the [owned lifecycle example](crate#examples).
    ///
    /// # Safety
    /// Platform glue must select an initialized, serviceable CPU, pin execution
    /// when selecting the current CPU, and refresh/notify the actual owner after
    /// acceptance. All delivery uses this collection's dispatch context contract.
    /// The handler must not unwind, sleep, enable IRQs or reenter timer dispatch.
    /// Deterministic core tests may model CPU service without programming hardware.
    pub unsafe fn start_on(
        &self,
        cpu: LogicalCpuId,
        deadline: MonotonicInstant,
    ) -> KResult<(LogicalCpuId, bool)> {
        let deadline = Deadline::new(deadline)?;
        // SAFETY: private Arc storage and static collection preserve addresses
        // and lifetime. Only owner-routed operations can access this node; the
        // caller supplies the platform placement and delivery guarantees.
        unsafe { self.timers.arm_on(&self.data.node, cpu, deadline) }
    }

    /// Starts a one-shot wait only if its deadline is still in the future.
    /// None means the locked clock check rejected it without changing this timer.
    /// Some reports the actual owner and newly added pending membership.
    /// Equality with the sampled clock is already due. Acceptance replaces any
    /// pending setting. It does not guarantee that the deadline is still future
    /// when this method returns.
    ///
    /// # Errors
    /// Returns [`kerrno::KError::InvalidInput`] if the deadline exceeds
    /// `u64::MAX` nanoseconds or `cpu` is outside this set, before mutation.
    /// An already elapsed valid deadline produces `Ok(None)`, not an error.
    ///
    /// # Examples
    /// See the [future-only start example](crate#examples).
    ///
    /// # Safety
    /// Same placement, delivery and notification obligations as [`Self::start_on`].
    /// `now_ns` must read a monotonic clock below `u64::MAX` without allocation,
    /// sleeping, unwinding, taking timer locks or calling business code.
    pub unsafe fn start_on_if_future(
        &self,
        cpu: LogicalCpuId,
        deadline: MonotonicInstant,
        now_ns: impl FnOnce() -> u64,
    ) -> KResult<Option<(LogicalCpuId, bool)>> {
        let deadline = Deadline::new(deadline)?;
        // SAFETY: the private Arc preserves node/handler storage. The caller
        // supplies placement, delivery and the constrained locked clock read.
        unsafe {
            self.timers
                .arm_on_if_future(&self.data.node, cpu, deadline, now_ns)
        }
    }

    /// Attempts cancellation without waiting for an in-flight callback.
    ///
    /// Running leaves pending successors untouched. This may be called from the
    /// callback itself; it returns Running.
    /// Takes the owner spinlock but never allocates, sleeps, or waits for delivery.
    /// Results describe the locked attempt; concurrent starts can immediately
    /// reactivate the timer. This does not authorize freeing callback state.
    /// May leave one stale hardware IRQ for the platform to re-arbitrate.
    pub fn try_cancel(&self) -> CancelResult {
        // SAFETY: the private allocation retains node and handler storage, and
        // all accesses use this static collection's revalidated owner lock.
        match unsafe { self.timers.try_cancel(&self.data.node) } {
            crate::cpu::CancelResult::Inactive => CancelResult::Inactive,
            crate::cpu::CancelResult::Removed(_) => CancelResult::Removed,
            crate::cpu::CancelResult::Running => CancelResult::Running,
        }
    }

    /// Waits for in-flight delivery and removes any pending successor.
    /// Returns whether a pending membership was removed. Does not allocate or
    /// sleep, or permanently disable the timer. Exclude concurrent starts for a
    /// durable stop; do not block the callback's progress or wait from itself.
    /// May leave one stale hardware IRQ, which the platform will re-arbitrate.
    pub fn cancel_sync(&self) -> bool {
        // SAFETY: the shell retains the private allocation and static set for
        // the entire wait; no raw node reference or direct mutation is exposed.
        // Progress violations can deadlock but cannot release live storage.
        unsafe { self.timers.cancel_sync(&self.data.node) }
    }
}

impl<H: TimerHandler + 'static, const N: usize> Drop for Timer<H, N> {
    fn drop(&mut self) {
        // &mut self covers the shell, not the distinct allocation still borrowed
        // by dispatch. Its fields are released only after cancellation completes.
        self.cancel_sync();
    }
}

#[unittest::mod_test]
mod tests {
    include!("../ut-tests/timer_unittest.rs");
}

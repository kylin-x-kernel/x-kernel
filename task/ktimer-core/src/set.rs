// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The CPU timer collection and its owner-routed operations.

use karch::cache::{CacheAligned, SMP_CACHE_BYTES};
use kcpu_id_map::LogicalCpuId;
use kerrno::{KError, KResult};
use kspin::{SpinNoIrq, SpinNoIrqGuard};

use super::{
    cpu::{CancelResult, CpuTimers},
    deadline::Deadline,
    index::TimerNode,
};

const MAX_DISPATCH_ROUNDS: usize = 3;
const MAX_DISPATCH_BACKOFF_NS: u64 = 100_000_000;

/// Owns the CPU timer data used by one timer subsystem instance.
/// Production can place one instance in static storage; tests own isolated sets.
/// `N` is the CPU slot count, not a limit on the number of registered timers.
/// CPU slots never grow, reorder or allocate after construction.
/// Preparing each owned [`Timer`](crate::timer::Timer) allocates storage; starting or cancelling
/// that prepared timer does not. This set does not provide a timer storage pool.
/// Nodes remain externally owned and may participate in only one set at a time.
/// See the [crate examples](crate#examples) for modeled setup and service.
pub struct TimerSet<const N: usize> {
    cpus: [CacheAligned<SpinNoIrq<CpuTimers>>; N],
}

const _: () = {
    assert!(align_of::<CacheAligned<SpinNoIrq<CpuTimers>>>() >= SMP_CACHE_BYTES);
    assert!(size_of::<CacheAligned<SpinNoIrq<CpuTimers>>>().is_multiple_of(SMP_CACHE_BYTES));
};

impl<const N: usize> TimerSet<N> {
    /// Creates empty CPU slots, not hardware/IRQ readiness.
    ///
    /// # Panics
    /// Panics for an empty CPU set because dormant nodes initially route to CPU 0.
    pub const fn new() -> Self {
        assert!(N > 0, "timer set requires at least one CPU");
        Self {
            cpus: [const { CacheAligned::new(SpinNoIrq::new(CpuTimers::new())) }; N],
        }
    }

    /// Supplies the next software-timer service deadline while keeping its CPU locked.
    /// During overload deferral this is later than the pending tree's minimum;
    /// individual timer deadlines remain unchanged.
    /// program combines it with other local deadline sources and writes hardware
    /// before returning; None means no prepared timer, not necessarily disarm.
    /// This neither dispatches callbacks nor consumes scheduler accounting.
    /// Does not cache the selected deadline or allocate a programming request.
    ///
    /// # Errors
    /// Returns [`KError::InvalidInput`] for an out-of-range CPU, without calling
    /// `program`.
    ///
    /// # Examples
    /// See the [locked programming example](crate#examples).
    ///
    /// # Safety
    /// Keep this set at a stable address and all participating nodes/handlers
    /// fixed and live until pending and running accesses have drained. All node
    /// operations must follow this set's owner-lock protocol; no direct tree or
    /// route mutation. A real hardware programmer must run on cpu with IRQs and
    /// migration disabled; deterministic test programmers may simulate that CPU.
    /// program, including capture cleanup, must not sleep, allocate, unwind,
    /// acquire timer/runqueue locks or invoke business callbacks. Complete local
    /// hardware arbitration/write within this call; never save the snapshot for
    /// a later write. If hardware writes are deferred, the final refresh must
    /// call this method again. Later remote advances still require notification.
    pub unsafe fn reprogram_local_timer(
        &self,
        cpu: LogicalCpuId,
        program: impl FnOnce(Option<u64>),
    ) -> KResult<()> {
        let timers = self
            .cpus
            .get(cpu.as_usize())
            .ok_or(KError::InvalidInput)?
            .lock();
        program(timers.service_deadline_ns());
        Ok(())
    }

    /// Delivers this CPU's currently due timers in deadline order.
    /// Each round selects deadlines against a fixed time snapshot.
    /// Up to three rounds catch deadlines reached during callback execution.
    /// If still overdue, defer software timer service by the time spent here,
    /// bounded to 1ns..=100ms. Stored deadlines do not
    /// change. Other platform deadline sources must remain independently armed.
    /// Returns whether any callback ran, including false during deferral.
    /// No due-batch allocation, hardware programming, or callbacks on other CPUs.
    /// Repeated immediate self-rearm can keep this loop running indefinitely.
    ///
    /// # Errors
    /// Returns [`KError::InvalidInput`] for an out-of-range CPU, without reading
    /// the clock.
    ///
    /// # Examples
    /// See the [owned lifecycle example](crate#examples) for modeled dispatch.
    ///
    /// # Safety
    /// Service the matching CPU without migration, nested or concurrent dispatch
    /// on that CPU. Production callers keep local IRQs disabled throughout and
    /// handlers must not enable them or reenter dispatch. Keep this set stable;
    /// preserve every node and handler at a fixed address through pending and
    /// running accesses, including callback registrations. All mutations must
    /// use this set's owner-lock protocol, with no direct tree or route changes.
    /// Callbacks must suit the context, must not unwind, free their storage or
    /// synchronously cancel themselves. now_ns must return nondecreasing monotonic
    /// times < u64::MAX, without allocating, sleeping, unwinding or reentering
    /// dispatch; it runs outside timer locks. CPU bounds are checked before calls.
    pub unsafe fn dispatch_due(
        &self,
        cpu: LogicalCpuId,
        mut now_ns: impl FnMut() -> u64,
    ) -> KResult<bool> {
        let timers = self.cpus.get(cpu.as_usize()).ok_or(KError::InvalidInput)?;
        let entry_ns = now_ns();
        let mut locked = timers.lock();
        if !locked.can_dispatch(entry_ns) {
            return Ok(false);
        }
        let mut round_ns = entry_ns;
        let mut any_due = false;
        for round in 0..MAX_DISPATCH_ROUNDS {
            // SAFETY: exclusive CPU service and live borrowed storage are
            // supplied by the caller. The guard belongs to this CPU's collection;
            // dispatch releases it before callbacks and returns without a lock.
            any_due |= unsafe { super::dispatch::dispatch_round(timers, round_ns, locked) };
            round_ns = now_ns();
            locked = timers.lock();
            if !locked.can_dispatch(round_ns) {
                break;
            }
            if round + 1 == MAX_DISPATCH_ROUNDS {
                // Repeated near-future rearm must not trap this IRQ or turn every
                // scheduler IRQ into another unbounded software-timer pass.
                let delay_ns = (round_ns - entry_ns).clamp(1, MAX_DISPATCH_BACKOFF_NS);
                locked.defer_dispatch_until(round_ns.saturating_add(delay_ns));
            }
        }
        Ok(any_due)
    }

    /// Cancels pending membership on the revalidated owner without waiting.
    /// Running leaves the callback and any successor untouched. No allocation,
    /// sleeping, handler calls or ownership release; valid in task/BH/hard IRQ.
    ///
    /// # Safety
    /// All node operations use this same stable TimerSet. Keep node
    /// storage fixed and live throughout this call and any pending/running accesses.
    /// A linked node must belong to its routed owner; exclude direct index mutation.
    pub(super) unsafe fn try_cancel(&self, node: &TimerNode) -> CancelResult {
        let (_, mut timers) = self.lock_owner(node);
        // SAFETY: route was revalidated under its lock; the caller guarantees
        // membership in this collection and stable, externally owned storage.
        unsafe { timers.try_cancel(node) }
    }

    /// Waits for delivery cleanup and cancels its pending successor.
    /// Returns whether this call removed a pending membership, not whether it waited.
    /// No allocation or sleeping. Valid in task/BH/hard IRQ when progress is possible.
    /// Does not permanently disable the node; callers may explicitly restart it.
    ///
    /// # Safety
    /// Uphold try_cancel's storage and owner-collection contract, including handler
    /// lifetime while running. Do not wait for this node from its own callback, hold
    /// a lock needed by the callback, or prevent the servicing CPU from progressing.
    /// Exclude concurrent external arms for a durable quiescence guarantee. Before
    /// freeing storage, also drain other external users; this waits only for timer
    /// delivery and removes pending membership, not arbitrary business references.
    pub(super) unsafe fn cancel_sync(&self, node: &TimerNode) -> bool {
        // SAFETY: forward the same lifetime/progress contract; spin_loop neither
        // allocates nor sleeps and is called only after releasing the owner lock.
        unsafe { self.cancel_sync_with(node, core::hint::spin_loop) }
    }

    /// Shared wait loop; tests drive delivery progress deterministically at retries.
    ///
    /// # Safety
    /// Same contract as cancel_sync. wait must preserve it, may not unwind, and must
    /// suit the calling context; it runs without a timer lock after observing running.
    unsafe fn cancel_sync_with(&self, node: &TimerNode, mut wait: impl FnMut()) -> bool {
        loop {
            let result = {
                let (_, mut timers) = self.lock_owner(node);
                // SAFETY: rechecked owner lock serializes this attempt. External
                // storage stays valid across retries and any owner-route change.
                unsafe { timers.try_cancel(node) }
            };
            match result {
                CancelResult::Inactive => return false,
                CancelResult::Removed(_) => return true,
                CancelResult::Running => wait(),
            }
        }
    }

    /// Locks the current owner and rechecks the route before returning any data.
    /// Private CPU slots retain their ordering throughout node participation.
    /// Safe lookup does not authorize unsafe node operations or extend its life.
    fn lock_owner(&self, node: &TimerNode) -> (LogicalCpuId, SpinNoIrqGuard<'_, CpuTimers>) {
        loop {
            let owner = node.owner_cpu();
            let timers = self.cpus[owner.as_usize()].lock();
            if node.owner_cpu() == owner {
                return (owner, timers);
            }
            // The old lock was acquired after a migration; drop it before retry.
        }
    }

    /// Arms on the selected CPU unless a delivery is still running on its owner.
    /// Returns the actual owner and whether a new pending membership was added.
    /// `selected_cpu` is an internal placement decision, not a public affinity API.
    /// If selected from the current CPU, the caller must pin that CPU for the
    /// complete operation. Notification/hardware readiness is not implemented here.
    ///
    /// # Safety
    /// All node operations use this same TimerSet, kept live and at a stable
    /// address throughout participation. Never route a node through another set.
    /// Keep the node at a fixed address and live through pending/execution accesses;
    /// exclude direct unsynchronized index/deadline/route operations. External owners
    /// preserve storage throughout participation; this layer manages no ownership.
    /// All selected slots must be eligible owners; construction alone does not
    /// establish hardware or IRQ readiness.
    pub(super) unsafe fn arm_on(
        &self,
        node: &TimerNode,
        selected_cpu: LogicalCpuId,
        deadline: Deadline,
    ) -> KResult<(LogicalCpuId, bool)> {
        // SAFETY: forwards the same storage and routing contract. Unconditional
        // acceptance cannot produce None and does not call any external code.
        unsafe { self.arm_on_checked(node, selected_cpu, deadline, || true) }.map(|armed| {
            #[expect(
                clippy::expect_used,
                reason = "CG-021-no-panic-on-reachable-failure: The acceptance closure always \
                          returns true, so successful arm_on_checked always returns Some"
            )]
            armed.expect("unconditional arm must be accepted")
        })
    }

    /// Arms only if the deadline is still in the future at the locked time check.
    /// None means already due (including equality): no setting, route, or
    /// pending successor was changed. Some has the same meaning as arm_on's result.
    /// Expiration after the check is an accepted timer, delivered asynchronously.
    /// This does not decide whether the calling task should actually block.
    ///
    /// # Safety
    /// Uphold arm_on's storage, placement and collection contract. now_ns must read
    /// the monotonic clock (< u64::MAX), without sleeping, allocating, unwinding,
    /// taking timer locks or calling business code. It runs once under the final
    /// owner lock (and both ordered locks during migration), before mutation.
    pub(super) unsafe fn arm_on_if_future(
        &self,
        node: &TimerNode,
        selected_cpu: LogicalCpuId,
        deadline: Deadline,
        now_ns: impl FnOnce() -> u64,
    ) -> KResult<Option<(LogicalCpuId, bool)>> {
        // SAFETY: same node/collection obligations, plus the caller's clock
        // contract. The check reads only immutable deadline data and the clock.
        unsafe {
            self.arm_on_checked(node, selected_cpu, deadline, || {
                deadline.deadline_ns() > now_ns()
            })
        }
    }

    /// Chooses and revalidates the final owner before checking and committing.
    ///
    /// # Safety
    /// Same contract as arm_on. accept must be nonblocking, nonallocating and
    /// nonreentrant, must not unwind, and must not acquire timer locks. It runs
    /// once under the final lock(s), never on route retries or invalid placement.
    unsafe fn arm_on_checked(
        &self,
        node: &TimerNode,
        selected_cpu: LogicalCpuId,
        deadline: Deadline,
        accept: impl FnOnce() -> bool,
    ) -> KResult<Option<(LogicalCpuId, bool)>> {
        if selected_cpu.as_usize() >= N {
            return Err(KError::InvalidInput);
        }
        loop {
            let (owner, mut timers) = self.lock_owner(node);
            if owner == selected_cpu || timers.is_running(node) {
                if !accept() {
                    return Ok(None);
                }
                // SAFETY: owner was checked under its lock. A linked node belongs
                // to that owner by the shared route/membership contract.
                return Ok(Some((owner, unsafe { timers.arm(node, deadline) })));
            }
            drop(timers);

            // Never upgrade a held single lock into a pair. Acquire low then high
            // so opposite-direction migrations cannot deadlock.
            let (low, high) = if owner < selected_cpu {
                (owner, selected_cpu)
            } else {
                (selected_cpu, owner)
            };
            let mut low_timers = self.cpus[low.as_usize()].lock();
            let mut high_timers = self.cpus[high.as_usize()].lock();
            if node.owner_cpu() != owner {
                continue;
            }
            let (old, new) = if owner == low {
                (&mut low_timers, &mut high_timers)
            } else {
                (&mut high_timers, &mut low_timers)
            };
            if !accept() {
                return Ok(None);
            }
            if old.is_running(node) {
                // SAFETY: the revalidated owner began delivery before both locks
                // were acquired. Preserve it and update under its existing lock.
                return Ok(Some((owner, unsafe { old.arm(node, deadline) })));
            }
            // SAFETY: both ordered locks are held and the old route is rechecked.
            // No delivery is running. Remove old membership before touching the
            // new index, retaining external storage throughout. Publish the route
            // only after insertion and before either lock is released.
            let was_pending = matches!(unsafe { old.try_cancel(node) }, CancelResult::Removed(_));
            // SAFETY: Cancellation removed the old membership and no delivery
            // runs; both owner locks still protect the retained stable node.
            unsafe { new.arm(node, deadline) };
            // SAFETY: Both ordered locks remain held after insertion; publish
            // the new route before either lock permits another node operation.
            unsafe { node.set_owner_cpu(selected_cpu) };
            return Ok(Some((selected_cpu, !was_pending)));
        }
    }
}

impl<const N: usize> Default for TimerSet<N> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(unittest)]
unittest::path_mod_test!(pub(crate) mod tests, "../ut-tests/set_unittest.rs");

#[cfg(test)]
#[path = "../tests/host/set_host.rs"]
mod slot_layout_tests;

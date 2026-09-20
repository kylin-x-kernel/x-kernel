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
        unsafe { self.arm_on_checked(node, selected_cpu, deadline, || true) }
            .map(|armed| armed.expect("unconditional arm must be accepted"))
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
            let was_pending = unsafe {
                let was_pending = matches!(old.try_cancel(node), CancelResult::Removed(_));
                new.arm(node, deadline);
                node.set_owner_cpu(selected_cpu);
                was_pending
            };
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
pub(crate) mod tests {
    use ktime_types::{MonotonicInstant, TimeSpan};
    use unittest::{assert, assert_eq, def_test};

    use super::*;
    use crate::cpu::CancelResult;

    fn setting(deadline: u64) -> Deadline {
        Deadline::new(MonotonicInstant::from_span_since_origin(
            TimeSpan::from_nanos(deadline),
        ))
        .unwrap()
    }

    #[def_test]
    fn reprogram_reads_each_current_deadline_under_the_cpu_lock() {
        let nodes = [TimerNode::new(setting(30)), TimerNode::new(setting(10))];
        let timer_set = TimerSet::<2>::new();
        let mut observed = [None; 6];
        #[cfg(feature = "smp")]
        let mut held_only_owner_lock = true;
        let mut refresh = |slot: usize| {
            // SAFETY: exclusive test driver simulates CPU1 programming. The
            // routine only writes fixed test storage and reads lock flags;
            // flags are observations, not synchronization or hardware evidence.
            unsafe {
                timer_set.reprogram_local_timer(1.into(), |deadline| {
                    observed[slot] = deadline;
                    #[cfg(feature = "smp")]
                    {
                        held_only_owner_lock &=
                            timer_set.cpus[1].is_locked() && !timer_set.cpus[0].is_locked();
                    }
                })
            }
            .unwrap();
        };
        refresh(0);
        // SAFETY: stable node array, same set, no concurrent delivery; both nodes
        // are cancelled before any external storage can be dropped.
        unsafe { timer_set.arm_on(&nodes[0], 1.into(), setting(30)) }.unwrap();
        refresh(1);
        // SAFETY: same set and lifetime; introduce an earlier pending node.
        unsafe { timer_set.arm_on(&nodes[1], 1.into(), setting(10)) }.unwrap();
        refresh(2);
        // SAFETY: no delivery is running; replacement selects CPU0.
        unsafe { timer_set.arm_on(&nodes[1], 0.into(), setting(40)) }.unwrap();
        refresh(3);
        let mut destination_deadline = None;
        // SAFETY: sequential modeled CPU0 programming only records current state.
        unsafe {
            timer_set.reprogram_local_timer(0.into(), |deadline| {
                destination_deadline = deadline;
            })
        }
        .unwrap();
        // SAFETY: no running callback; remove the current earliest node.
        unsafe { timer_set.cancel_sync(&nodes[0]) };
        refresh(4);
        // SAFETY: drain the final borrowed membership before assertions.
        unsafe { timer_set.cancel_sync(&nodes[1]) };
        refresh(5);
        assert_eq!(observed, [None, Some(30), Some(10), Some(30), None, None]);
        assert_eq!(destination_deadline, Some(40));
        #[cfg(feature = "smp")]
        assert!(held_only_owner_lock);
        assert!(nodes.iter().all(|node| !node.is_linked()));
    }

    #[def_test]
    fn reprogram_preserves_zero_horizon_and_other_deadline_sources() {
        let node = TimerNode::new(setting(0));
        let timer_set = TimerSet::<1>::new();
        for deadline in [0, 10, u64::MAX] {
            // SAFETY: fixed external node and one set, with no concurrent service.
            unsafe { timer_set.arm_on(&node, 0.into(), setting(deadline)) }.unwrap();
            let mut prepared = None;
            let mut programmed = None;
            // SAFETY: deterministic CPU0 programming stub with an independent
            // local source at 20; only fixed-size values are read/written.
            unsafe {
                timer_set.reprogram_local_timer(0.into(), |earliest| {
                    prepared = earliest;
                    programmed = [earliest, Some(20)].into_iter().flatten().min();
                })
            }
            .unwrap();
            // SAFETY: no running delivery; drain before assertions may return.
            unsafe { timer_set.cancel_sync(&node) };
            assert_eq!(prepared, Some(deadline));
            assert_eq!(programmed, Some(deadline.min(20)));
        }
        for other in [Some(20), None] {
            let mut programmed = Some(99);
            // SAFETY: empty prepared set; the local programming stub must still
            // account for other sources rather than unconditionally disarming.
            unsafe {
                timer_set.reprogram_local_timer(0.into(), |earliest| {
                    programmed = [earliest, other].into_iter().flatten().min();
                })
            }
            .unwrap();
            assert_eq!(programmed, other);
        }
    }

    #[def_test]
    fn reprogram_invalid_cpu_does_not_call_programmer() {
        let timer_set = TimerSet::<1>::new();
        let mut called = false;
        // SAFETY: invalid CPU is rejected before the nonblocking local stub runs.
        let result = unsafe { timer_set.reprogram_local_timer(1.into(), |_| called = true) };
        assert_eq!(result, Err(KError::InvalidInput));
        assert!(!called);
    }

    #[def_test]
    fn dispatch_due_orders_local_timers_and_keeps_future_and_remote_pending() {
        use core::sync::atomic::{AtomicUsize, Ordering};

        use crate::{control::TimerControl, deadline::Expiration};

        let timer_set = TimerSet::<2>::new();
        let order = AtomicUsize::new(0);
        let handler = |_: TimerControl<'_>, event: Expiration| {
            let next = order.load(Ordering::Relaxed) * 10 + event.deadline_ns as usize / 10;
            order.store(next, Ordering::Relaxed);
        };
        // SAFETY: one stable external handler outlives the fixed node array;
        // all pending memberships are removed before any storage is released.
        let nodes = [30, 10, 20, 40, 5]
            .map(|deadline| unsafe { TimerNode::with_handler(setting(deadline), &handler) });
        for (index, node) in nodes.iter().enumerate() {
            let cpu = usize::from(index == 4);
            // SAFETY: each fixed node uses this same stable set, with no delivery
            // until preparation completes. Original deadlines remain under this lock.
            unsafe { timer_set.arm_on(node, cpu.into(), setting([30, 10, 20, 40, 5][index])) }
                .unwrap();
        }
        // SAFETY: deterministic single-thread driver for CPU0 only, with stable
        // storage and a valid monotonic clock. No hardware/IRQ-context claim.
        let delivered = unsafe { timer_set.dispatch_due(0.into(), || 35) };
        let local_pending = timer_set.cpus[0].lock().earliest_deadline_ns();
        let remote_pending = timer_set.cpus[1].lock().earliest_deadline_ns();
        for node in &nodes {
            // SAFETY: all callbacks completed; drain pending before assertions.
            unsafe { timer_set.cancel_sync(node) };
        }
        assert_eq!(delivered, Ok(true));
        assert_eq!(order.load(Ordering::Relaxed), 123);
        assert_eq!(local_pending, Some(40));
        assert_eq!(remote_pending, Some(5));
    }

    #[def_test]
    fn dispatch_due_rechecks_time_and_self_rearm_after_cleanup() {
        use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

        use crate::{control::TimerControl, deadline::Expiration};

        struct Cleanup<'a>(&'a AtomicUsize);
        impl Drop for Cleanup<'_> {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let timer_set = TimerSet::<1>::new();
        let now = AtomicU64::new(10);
        let order = AtomicUsize::new(0);
        let cleanups = AtomicUsize::new(0);
        let observed_cleanups = AtomicUsize::new(0);
        let handler = |timer: TimerControl<'_>, event: Expiration| {
            let _cleanup = Cleanup(&cleanups);
            order.store(
                order.load(Ordering::Relaxed) * 100 + event.deadline_ns as usize,
                Ordering::Relaxed,
            );
            if event.deadline_ns == 10 {
                timer
                    .rearm(MonotonicInstant::from_span_since_origin(
                        TimeSpan::from_nanos(20),
                    ))
                    .unwrap();
                now.store(25, Ordering::Relaxed);
            } else if event.deadline_ns == 20 {
                observed_cleanups.store(cleanups.load(Ordering::Relaxed), Ordering::Relaxed);
            }
        };
        let settings = [
            Deadline::new(MonotonicInstant::from_span_since_origin(
                TimeSpan::from_nanos(10),
            ))
            .unwrap(),
            Deadline::new(MonotonicInstant::from_span_since_origin(
                TimeSpan::from_nanos(15),
            ))
            .unwrap(),
        ];
        // SAFETY: handler and captures stay stable until both nodes are drained.
        let nodes = settings.map(|setting| unsafe { TimerNode::with_handler(setting, &handler) });
        for (node, setting) in nodes.iter().zip(settings) {
            // SAFETY: fixed node array and one set; setup precedes all dispatch.
            unsafe { timer_set.arm_on(node, 0.into(), setting) }.unwrap();
        }
        // SAFETY: deterministic exclusive CPU0 service, monotonic clock updated
        // only by the callback. Callback-local cleanup ends before the next call.
        let delivered = unsafe { timer_set.dispatch_due(0.into(), || now.load(Ordering::Relaxed)) };
        let pending = timer_set.cpus[0].lock().earliest_deadline_ns();
        for node in &nodes {
            // SAFETY: service returned; remove any failed-test residue before Drop.
            unsafe { timer_set.cancel_sync(node) };
        }
        assert_eq!(delivered, Ok(true));
        assert_eq!(order.load(Ordering::Relaxed), 101520);
        assert_eq!(observed_cleanups.load(Ordering::Relaxed), 2);
        assert_eq!(cleanups.load(Ordering::Relaxed), 3);
        assert_eq!(pending, None);
    }

    #[def_test]
    fn dispatch_due_empty_and_invalid_cpu_do_not_deliver() {
        let timer_set = TimerSet::<1>::new();
        let mut clock_calls = 0;
        // SAFETY: empty set with an exclusive test driver and nonblocking clock.
        let empty = unsafe {
            timer_set.dispatch_due(0.into(), || {
                clock_calls += 1;
                10
            })
        };
        assert_eq!(empty, Ok(false));
        assert_eq!(clock_calls, 1);
        // SAFETY: invalid CPU is rejected before invoking the supplied clock.
        let invalid = unsafe {
            timer_set.dispatch_due(1.into(), || {
                clock_calls += 1;
                10
            })
        };
        assert_eq!(invalid, Err(KError::InvalidInput));
        assert_eq!(clock_calls, 1);
    }

    #[def_test]
    fn independent_sets_keep_cpu_state_separate() {
        let nodes = [TimerNode::new(setting(10)), TimerNode::new(setting(20))];
        let first = TimerSet::<1>::new();
        let second = TimerSet::<1>::new();
        // SAFETY: distinct stable nodes are each used with exactly one set;
        // both sets are drained before any external node storage is released.
        unsafe {
            first.arm_on(&nodes[0], 0.into(), setting(10)).unwrap();
            second.arm_on(&nodes[1], 0.into(), setting(20)).unwrap();
        }
        // SAFETY: first owns this node's membership, with no running callback.
        let removed_first = unsafe { first.cancel_sync(&nodes[0]) };
        let first_deadline = first.cpus[0].lock().earliest_deadline_ns();
        let second_deadline = second.cpus[0].lock().earliest_deadline_ns();
        // SAFETY: second owns the other node; drain it before assertions.
        let removed_second = unsafe { second.cancel_sync(&nodes[1]) };
        assert!(removed_first && removed_second);
        assert_eq!(first_deadline, None);
        assert_eq!(second_deadline, Some(20));
        assert!(nodes.iter().all(|node| !node.is_linked()));
    }

    #[def_test]
    fn expired_wait_start_preserves_idle_route_and_pending_deadline() {
        for selected in [0, 1] {
            let node = TimerNode::new(setting(100));
            let timer_set = TimerSet::<2>::new();
            for deadline in [9, 10] {
                let mut clock_calls = 0;
                // SAFETY: stable idle node and CPU collection. Clock only records
                // local observations; no timer mutations or locks inside it.
                let result = unsafe {
                    timer_set.arm_on_if_future(&node, selected.into(), setting(deadline), || {
                        clock_calls += 1;
                        10
                    })
                };
                assert_eq!(result, Ok(None));
                assert_eq!(clock_calls, 1);
                assert_eq!(node.owner_cpu(), 0.into());
                assert!(!node.is_linked());
            }
            // SAFETY: same storage and initialized placement; preserve this deadline
            // when subsequent already-due replacements are rejected.
            unsafe { timer_set.arm_on(&node, selected.into(), setting(100)) }.unwrap();
            // SAFETY: matching stable collection; the clock is a constant sample.
            let result = unsafe {
                timer_set.arm_on_if_future(&node, (1 - selected).into(), setting(10), || 10)
            };
            let (owner, mut timers) = timer_set.lock_owner(&node);
            // SAFETY: the owner lock serializes deadline access to this live node.
            let retained = unsafe { node.deadline() };
            // SAFETY: no delivery in this test; unlink before assertions can exit.
            unsafe { timers.try_cancel(&node) };
            assert_eq!(result, Ok(None));
            assert_eq!(owner, selected.into());
            assert_eq!(retained, setting(100));
        }
    }

    #[def_test]
    fn wait_start_checks_once_under_final_locks_and_expires_normally() {
        use core::sync::atomic::{AtomicUsize, Ordering};

        use crate::{control::TimerControl, deadline::Expiration, dispatch};

        for selected in [0, 1] {
            let calls = AtomicUsize::new(0);
            let handler = |_: TimerControl<'_>, _: Expiration| {
                calls.fetch_add(1, Ordering::Relaxed);
            };
            // SAFETY: fixed handler and captures outlive node and delivery.
            let node = unsafe { TimerNode::with_handler(setting(10), &handler) };
            let timer_set = TimerSet::<2>::new();
            let mut clock_calls = 0;
            #[cfg(feature = "smp")]
            let mut held_final_locks = false;
            // SAFETY: stable external storage; this clock reads lock flags only
            // as a test observation, not synchronization, and takes no locks.
            let result = unsafe {
                timer_set.arm_on_if_future(&node, selected.into(), setting(10), || {
                    clock_calls += 1;
                    #[cfg(feature = "smp")]
                    {
                        held_final_locks =
                            timer_set.cpus[0].is_locked() && timer_set.cpus[selected].is_locked();
                    }
                    9
                })
            };
            let before_dispatch = calls.load(Ordering::Relaxed);
            // SAFETY: explicitly drive this node's owner; external storage is
            // live. Time reached the deadline after acceptance, before service.
            let delivered = unsafe {
                dispatch::dispatch_round(
                    &timer_set.cpus[selected],
                    10,
                    timer_set.cpus[selected].lock(),
                )
            };
            assert_eq!(result, Ok(Some((selected.into(), true))));
            assert_eq!(clock_calls, 1);
            #[cfg(feature = "smp")]
            assert!(held_final_locks);
            assert_eq!(before_dispatch, 0);
            assert!(delivered);
            assert_eq!(calls.load(Ordering::Relaxed), 1);
            assert!(!node.is_linked());
        }
    }

    #[def_test]
    fn rejected_wait_start_keeps_running_successor() {
        let node = TimerNode::new(setting(10));
        let timer_set = TimerSet::<2>::new();
        // SAFETY: stable local storage and consistent owner-lock protocol.
        unsafe { timer_set.arm_on(&node, 1.into(), setting(10)) }.unwrap();
        let running = timer_set.cpus[1].lock().take_expired(10).unwrap().0;
        // SAFETY: same running timer; install a successor on its existing owner.
        unsafe { timer_set.arm_on(&node, 0.into(), setting(100)) }.unwrap();
        // SAFETY: live storage, initialized modeled CPUs, constant locked clock.
        let result = unsafe { timer_set.arm_on_if_future(&node, 0.into(), setting(15), || 15) };
        let mut timers = timer_set.cpus[1].lock();
        assert_eq!(result, Ok(None));
        assert_eq!(node.owner_cpu(), 1.into());
        assert_eq!(timers.earliest_deadline_ns(), Some(100));
        let address = intrusive_collections::UnsafeRef::into_raw(running).addr();
        // SAFETY: modeled callback and delivery borrows have ended.
        unsafe { timers.clear_running(address) };
        assert!(matches!(
            // SAFETY: same live pending node; drain before stack storage ends.
            unsafe { timers.try_cancel(&node) },
            CancelResult::Removed(_)
        ));
    }

    #[def_test]
    fn wait_start_invalid_cpu_skips_clock_but_ordinary_arm_accepts_past() {
        let node = TimerNode::new(setting(10));
        let timer_set = TimerSet::<1>::new();
        let mut clock_calls = 0;
        // SAFETY: stable node and collection; invalid placement must exit before
        // the nonblocking test clock or any node mutation.
        let result = unsafe {
            timer_set.arm_on_if_future(&node, 1.into(), setting(10), || {
                clock_calls += 1;
                10
            })
        };
        assert_eq!(result, Err(KError::InvalidInput));
        assert_eq!(clock_calls, 0);
        for deadline in [9, 10] {
            // SAFETY: fixed node, initialized CPU and serialized access. Ordinary
            // arm must not acquire the wait-specific rejection behavior.
            let result = unsafe { timer_set.arm_on(&node, 0.into(), setting(deadline)) };
            // SAFETY: serve the matching test CPU with live callback-free storage.
            let delivered = unsafe {
                crate::dispatch::dispatch_round(&timer_set.cpus[0], 10, timer_set.cpus[0].lock())
            };
            assert_eq!(result, Ok((0.into(), true)));
            assert!(delivered);
            assert!(!node.is_linked());
        }
    }

    #[def_test]
    fn routed_cancellation_follows_owner_and_allows_restart() {
        let node = TimerNode::new(setting(10));
        let timer_set = TimerSet::<2>::new();
        // SAFETY: stack node outlives this collection; only routed operations
        // access it and no delivery can occur in this sequential test.
        assert!(!unsafe { timer_set.cancel_sync(&node) });
        for owner in [1, 0, 1] {
            // SAFETY: same fixed node and CPU-ordered collection, now idle.
            unsafe { timer_set.arm_on(&node, owner.into(), setting(10)) }.unwrap();
            assert!(matches!(
                // SAFETY: same collection owns this stable node at the new route.
                unsafe { timer_set.try_cancel(&node) },
                CancelResult::Removed(_)
            ));
            assert_eq!(timer_set.cpus[owner].lock().earliest_deadline_ns(), None);
            // SAFETY: cancelled node remains live and reusable.
            unsafe { timer_set.arm_on(&node, owner.into(), setting(20)) }.unwrap();
            // SAFETY: the pending node has no callback or competing users.
            assert!(unsafe { timer_set.cancel_sync(&node) });
            assert!(!node.is_linked());
        }
    }

    #[def_test]
    fn sync_cancel_releases_lock_and_waits_through_cleanup() {
        for has_successor in [false, true] {
            let initial = Deadline::new(MonotonicInstant::from_span_since_origin(
                TimeSpan::from_nanos(10),
            ))
            .unwrap();
            let node = TimerNode::new(initial);
            let timer_set = TimerSet::<2>::new();
            // SAFETY: stable node outlives both CPUs; all accesses use their locks.
            unsafe { timer_set.arm_on(&node, 1.into(), initial) }.unwrap();
            let mut running = Some(timer_set.cpus[1].lock().take_expired(10).unwrap().0);
            if has_successor {
                // SAFETY: same node/collection; a running node must keep owner 1.
                unsafe { timer_set.arm_on(&node, 0.into(), setting(20)) }.unwrap();
            }
            assert!(matches!(
                // SAFETY: same stable node. Non-waiting cancel must retain successor.
                unsafe { timer_set.try_cancel(&node) },
                CancelResult::Running
            ));
            assert_eq!(
                timer_set.cpus[1].lock().earliest_deadline_ns(),
                has_successor.then_some(20)
            );
            let mut waits = 0;
            let mut cleanup_checks_passed = true;
            let progress = || {
                waits += 1;
                // Taking this lock proves cancellation did not keep it while waiting.
                let mut timers = timer_set.cpus[1].lock();
                if waits == 1 {
                    // Callback returned, but its local cleanup still owns this delivery.
                    cleanup_checks_passed &= timers.is_running(&node);
                } else {
                    cleanup_checks_passed &= waits == 2;
                    let address =
                        intrusive_collections::UnsafeRef::into_raw(running.take().unwrap()).addr();
                    // SAFETY: emulated callback cleanup and delivery borrows ended;
                    // external stack storage remains live. No callback dereference follows.
                    unsafe { timers.clear_running(address) };
                }
            };
            // SAFETY: deterministic progress completes the delivery under its
            // owner lock; no external arm races, and all storage stays live.
            let removed = unsafe { timer_set.cancel_sync_with(&node, progress) };
            assert_eq!(waits, 2);
            assert!(cleanup_checks_passed);
            assert_eq!(removed, has_successor);
            assert!(!node.is_linked());
            assert!(!timer_set.cpus[1].lock().is_running(&node));
            // SAFETY: fully quiescent node can move to another owner and restart.
            unsafe { timer_set.arm_on(&node, 0.into(), setting(30)) }.unwrap();
            // SAFETY: new pending membership on owner 0, with no delivery running.
            assert!(unsafe { timer_set.cancel_sync(&node) });
        }
    }

    #[def_test]
    fn idle_and_pending_restarts_select_the_requested_cpu() {
        let node = TimerNode::new(setting(10));
        let timer_set = TimerSet::<2>::new();
        for selected in [1, 0, 1, 0] {
            // SAFETY: the stable stack node outlives both locks; all accesses
            // use this collection and borrowed storage needs no owner holds.
            let (owner, inserted) =
                unsafe { timer_set.arm_on(&node, selected.into(), setting(10)) }.unwrap();
            assert_eq!(owner.as_usize(), selected);
            assert!(inserted);
            // SAFETY: same live node and lock collection; no delivery is running.
            let (owner, inserted) =
                unsafe { timer_set.arm_on(&node, (1 - selected).into(), setting(20)) }.unwrap();
            assert_eq!(owner.as_usize(), 1 - selected);
            assert!(!inserted);
            assert_eq!(timer_set.cpus[selected].lock().earliest_deadline_ns(), None);
            let (owner, mut timers) = timer_set.lock_owner(&node);
            assert_eq!(owner.as_usize(), 1 - selected);
            assert_eq!(timers.earliest_deadline_ns(), Some(20));
            assert!(matches!(
                // SAFETY: revalidated owner lock held; borrowed removed hold is empty.
                unsafe { timers.try_cancel(&node) },
                CancelResult::Removed(_)
            ));
        }
        assert!(
            timer_set
                .cpus
                .iter()
                .all(|cpu| cpu.lock().earliest_deadline_ns().is_none())
        );
        assert_eq!(
            // SAFETY: same collection; invalid target must leave the idle node alone.
            unsafe { timer_set.arm_on(&node, 2.into(), setting(99)) },
            Err(KError::InvalidInput)
        );
        assert_eq!(node.owner_cpu().as_usize(), 1);
    }

    #[def_test]
    fn running_and_cleanup_prevent_owner_migration() {
        let node = TimerNode::new(setting(10));
        let timer_set = TimerSet::<2>::new();
        // SAFETY: stack-stable borrowed node, only this collection accesses it.
        unsafe { timer_set.arm_on(&node, 1.into(), setting(10)) }.unwrap();
        let running = {
            let (_, mut timers) = timer_set.lock_owner(&node);
            timers.take_expired(10).unwrap().0
        };
        assert_eq!(
            // SAFETY: running remains live; explicit rearm must keep the old owner.
            unsafe { timer_set.arm_on(&node, 0.into(), setting(20)) }
                .unwrap()
                .0
                .as_usize(),
            1
        );
        {
            let (_, mut timers) = timer_set.lock_owner(&node);
            assert!(matches!(
                // SAFETY: matching running node and owner lock; no owner releases.
                unsafe { timers.try_cancel(&node) },
                CancelResult::Running
            ));
        }
        assert_eq!(
            // SAFETY: even after callback completion, cleanup keeps the old owner.
            unsafe { timer_set.arm_on(&node, 0.into(), setting(30)) }
                .unwrap()
                .0
                .as_usize(),
            1
        );
        let address = intrusive_collections::UnsafeRef::into_raw(running).addr();
        {
            let (_, mut timers) = timer_set.lock_owner(&node);
            // SAFETY: borrowed execution accesses finished; only saved identity
            // is needed to retire it, then cancellation removes the successor.
            unsafe {
                timers.clear_running(address);
                timers.try_cancel(&node);
            }
        }
        assert_eq!(
            // SAFETY: now idle; same stable node may move to the selected slot.
            unsafe { timer_set.arm_on(&node, 0.into(), setting(40)) }
                .unwrap()
                .0
                .as_usize(),
            0
        );
        let (_, mut timers) = timer_set.lock_owner(&node);
        // SAFETY: validated owner lock; remove the last borrowed membership.
        unsafe { timers.try_cancel(&node) };
    }

    /// Runs the shared-node race with two workers scheduled and joined by the host.
    /// Workers must run concurrently; yield_now must yield without timer locks.
    ///
    /// # Returns
    ///
    /// Returns `TestResult::Failed` if the driver fails, a worker does not finish,
    /// routing fails, or pending membership remains after cancellation. The
    /// `unittest` assertions return failure rather than panicking.
    ///
    /// # Panics
    ///
    /// Propagates panics from the supplied worker driver or yield function; this
    /// helper does not catch them.
    #[cfg(feature = "smp")]
    pub fn concurrent_owner_routes(
        run_workers: impl FnOnce([alloc::boxed::Box<dyn FnOnce() + Send>; 2]) -> bool,
        yield_now: fn(),
    ) -> unittest::TestResult {
        use alloc::{boxed::Box, sync::Arc};
        use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        let node = Arc::new(TimerNode::new(setting(10)));
        let timer_set = Arc::new(TimerSet::<2>::new());
        let ready = Arc::new(AtomicUsize::new(0));
        let finished = Arc::new(AtomicUsize::new(0));
        let failed = Arc::new(AtomicBool::new(false));
        let workers: [Box<dyn FnOnce() + Send>; 2] = core::array::from_fn(|cpu| {
            let node = node.clone();
            let timer_set = timer_set.clone();
            let ready = ready.clone();
            let finished = finished.clone();
            let failed = failed.clone();
            Box::new(move || {
                ready.fetch_add(1, Ordering::AcqRel);
                while ready.load(Ordering::Acquire) != 2 {
                    yield_now();
                }
                for iteration in 0..512 {
                    // SAFETY: scenario and workers hold the same stable Arc
                    // node/lock collection. No direct mutation or dispatch
                    // occurs; all pending references are borrowed.
                    let result =
                        unsafe { timer_set.arm_on(&node, cpu.into(), setting(10 + iteration)) };
                    if result.is_err() {
                        failed.store(true, Ordering::Release);
                    }
                    {
                        let (owner, mut timers) = timer_set.lock_owner(&node);
                        if owner != node.owner_cpu() {
                            failed.store(true, Ordering::Release);
                        }
                        // SAFETY: validated owner lock and live borrowed node;
                        // strong storage references outlive both workers.
                        unsafe { timers.try_cancel(&node) };
                    }
                    yield_now();
                }
                finished.fetch_add(1, Ordering::AcqRel);
            }) as Box<dyn FnOnce() + Send>
        });
        assert!(run_workers(workers));
        assert_eq!(finished.load(Ordering::Acquire), 2);
        assert!(!failed.load(Ordering::Acquire));
        assert!(!node.is_linked());
        assert!(
            timer_set
                .cpus
                .iter()
                .all(|cpu| cpu.lock().earliest_deadline_ns().is_none())
        );
        unittest::TestResult::Ok
    }

    /// Drives actual dispatch and cancellation on workers pinned to CPUs 0/1.
    /// The driver must join both workers before returning; yield_now is used
    /// only at the startup barrier, before the callback/cancellation handshake.
    ///
    /// # Returns
    ///
    /// Returns `TestResult::Failed` if the driver fails or the checks detect
    /// premature cancellation completion, remaining timer state, or incorrect
    /// ownership cleanup. The `unittest` assertions return failure rather than
    /// panicking.
    ///
    /// # Panics
    ///
    /// Panics if initial arming on CPU 0 unexpectedly fails (`unwrap`); the
    /// scenario provides a fresh node and a two-CPU timer set. Panics from the
    /// supplied worker driver or yield function are not caught.
    #[cfg(feature = "smp")]
    pub fn concurrent_cancel_waits_for_cleanup(
        run_workers: impl FnOnce([alloc::boxed::Box<dyn FnOnce() + Send>; 2]) -> bool,
        yield_now: fn(),
    ) -> unittest::TestResult {
        use alloc::{boxed::Box, sync::Arc};
        use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        use crate::{
            control::TimerControl, deadline::Expiration, dispatch::dispatch_round,
            index::TimerHandler,
        };

        struct Scenario {
            timer_set: TimerSet<2>,
            ready: AtomicUsize,
            // 1: callback active, 2: local cleanup active, 3: cleanup complete.
            stage: AtomicUsize,
            wait_seen: AtomicUsize,
            cancel_returned: AtomicBool,
            failed: AtomicBool,
            drops: Arc<AtomicUsize>,
        }
        impl Drop for Scenario {
            fn drop(&mut self) {
                self.drops.fetch_add(1, Ordering::Relaxed);
            }
        }
        struct Cleanup<'a>(&'a Scenario);
        impl Drop for Cleanup<'_> {
            fn drop(&mut self) {
                self.0.stage.store(2, Ordering::Release);
                while self.0.wait_seen.load(Ordering::Acquire) < 2 {
                    core::hint::spin_loop();
                }
                if self.0.cancel_returned.load(Ordering::Acquire) {
                    self.0.failed.store(true, Ordering::Relaxed);
                }
                self.0.stage.store(3, Ordering::Release);
            }
        }
        impl TimerHandler for Scenario {
            fn on_expiration(&self, timer: TimerControl<'_>, _: Expiration) {
                let _cleanup = Cleanup(self);
                let rearmed = timer.rearm(MonotonicInstant::from_span_since_origin(
                    TimeSpan::from_nanos(20),
                ));
                if rearmed.is_err() {
                    self.failed.store(true, Ordering::Relaxed);
                }
                self.stage.store(1, Ordering::Release);
                while self.wait_seen.load(Ordering::Acquire) < 1 {
                    core::hint::spin_loop();
                }
                if self.cancel_returned.load(Ordering::Acquire) {
                    self.failed.store(true, Ordering::Relaxed);
                }
            }
        }
        let drops = Arc::new(AtomicUsize::new(0));
        let scenario = Arc::new(Scenario {
            timer_set: TimerSet::new(),
            ready: AtomicUsize::new(0),
            stage: AtomicUsize::new(0),
            wait_seen: AtomicUsize::new(0),
            cancel_returned: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            drops: drops.clone(),
        });
        // SAFETY: the outer Arc keeps handler storage fixed and shared until
        // both workers have joined and node storage has been released.
        let node = Arc::new(unsafe { TimerNode::with_handler(setting(10), scenario.as_ref()) });
        // SAFETY: both allocations remain live through dispatch/cancel; all
        // operations use this same stable CPU collection and routed owner.
        unsafe { scenario.timer_set.arm_on(&node, 0.into(), setting(10)) }.unwrap();
        let workers: [Box<dyn FnOnce() + Send>; 2] = core::array::from_fn(|cpu| {
            let scenario = scenario.clone();
            let node = node.clone();
            Box::new(move || {
                scenario.ready.fetch_add(1, Ordering::AcqRel);
                while scenario.ready.load(Ordering::Acquire) != 2 {
                    yield_now();
                }
                if cpu == 0 {
                    // SAFETY: driver pins this worker to owner CPU 0, with
                    // external node/handler ownership spanning the entire call.
                    if !unsafe {
                        dispatch_round(
                            &scenario.timer_set.cpus[0],
                            10,
                            scenario.timer_set.cpus[0].lock(),
                        )
                    } {
                        scenario.failed.store(true, Ordering::Relaxed);
                    }
                } else {
                    while scenario.stage.load(Ordering::Acquire) == 0 {
                        core::hint::spin_loop();
                    }
                    // SAFETY: callback is held at stage 1 on another CPU;
                    // shared node and collection remain externally owned.
                    let result = unsafe { scenario.timer_set.try_cancel(&node) };
                    if !matches!(result, CancelResult::Running) {
                        scenario.failed.store(true, Ordering::Relaxed);
                    }
                    let observe_wait = || {
                        let stage = scenario.stage.load(Ordering::Acquire);
                        if stage == 2 && scenario.wait_seen.load(Ordering::Relaxed) < 2 {
                            let (_, timers) = scenario.timer_set.lock_owner(&node);
                            if !timers.is_running(&node)
                                || timers.earliest_deadline_ns() != Some(20)
                            {
                                scenario.failed.store(true, Ordering::Relaxed);
                            }
                        }
                        scenario.wait_seen.store(stage, Ordering::Release);
                        core::hint::spin_loop();
                    };
                    // SAFETY: worker 0 can independently finish; no timer lock
                    // is retained across waits. The observer only acknowledges
                    // stages and checks locked state; it cannot finish delivery.
                    let removed =
                        unsafe { scenario.timer_set.cancel_sync_with(&node, observe_wait) };
                    if !removed || scenario.stage.load(Ordering::Acquire) != 3 {
                        scenario.failed.store(true, Ordering::Relaxed);
                    }
                    scenario.cancel_returned.store(true, Ordering::Release);
                }
            }) as Box<dyn FnOnce() + Send>
        });
        let workers_succeeded = run_workers(workers);
        let was_quiescent = !node.is_linked()
            && scenario.timer_set.cpus.iter().all(|cpu| {
                let timers = cpu.lock();
                !timers.is_running(&node) && timers.earliest_deadline_ns().is_none()
            });
        // Both workers have joined. Unlink any failed-test residue while node
        // storage is still live, before a failing assertion can return early.
        // Preserve the pre-cleanup observation so this cannot hide a failure.
        for cpu in &scenario.timer_set.cpus {
            *cpu.lock() = CpuTimers::new();
        }
        assert!(workers_succeeded);
        assert!(scenario.cancel_returned.load(Ordering::Acquire));
        assert!(!scenario.failed.load(Ordering::Relaxed));
        assert!(was_quiescent);
        assert_eq!(Arc::strong_count(&node), 1);
        assert_eq!(Arc::strong_count(&scenario), 1);
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        drop(node);
        drop(scenario);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        unittest::TestResult::Ok
    }
}

#[cfg(test)]
mod slot_layout_tests {
    use super::*;

    #[test]
    fn cpu_slots_follow_shared_cache_alignment() {
        let timers = TimerSet::<4>::new();
        for pair in timers.cpus.windows(2) {
            let first = core::ptr::from_ref(&pair[0]).addr();
            let next = core::ptr::from_ref(&pair[1]).addr();
            assert_eq!(first % SMP_CACHE_BYTES, 0);
            assert_eq!(next - first, size_of_val(&pair[0]));
            assert!((first + size_of_val(&pair[0]) - 1) / SMP_CACHE_BYTES < next / SMP_CACHE_BYTES);
        }
    }
}

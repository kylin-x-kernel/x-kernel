// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-CPU retirement segments, controlled by one serialized period driver.

use core::sync::atomic::{Ordering, fence};

use intrusive_collections::{LinkedList, UnsafeRef};
use kspin::{SpinNoIrq, SpinNoIrqGuard};

use crate::{
    PeriodId,
    callback::{CallbackAdapter, CallbackNode},
    grace_period::GracePeriodCoordinator,
};

type CallbackList = LinkedList<CallbackAdapter>;

struct WaitingSegment {
    period: Option<PeriodId>,
    callbacks: CallbackList,
}

impl WaitingSegment {
    const fn new() -> Self {
        Self {
            period: None,
            callbacks: CallbackList::new(CallbackAdapter::NEW),
        }
    }
}

struct QueueState {
    unassigned: CallbackList,
    // Only the current and its successor period can have assigned callbacks.
    waiting: [WaitingSegment; 2],
    eligible: CallbackList,
    is_executing: bool,
    outstanding: usize,
}

/// Caller-retained retirement storage for one CPU; no fixed callback capacity.
///
/// Producers touch only this queue. Only CallbackDriver assigns/promotes work.
/// Queue destruction does not cancel or invoke accepted callbacks: its owner
/// must retain it and all submitted storage until execution completes.
pub(super) struct CallbackQueue {
    state: SpinNoIrq<QueueState>,
}

impl CallbackQueue {
    pub(super) const fn new() -> Self {
        Self {
            state: SpinNoIrq::new(QueueState {
                unassigned: CallbackList::new(CallbackAdapter::NEW),
                waiting: [WaitingSegment::new(), WaitingSegment::new()],
                eligible: CallbackList::new(CallbackAdapter::NEW),
                is_executing: false,
                outstanding: 0,
            }),
        }
    }

    fn lock(&self) -> SpinNoIrqGuard<'_, QueueState> {
        let state = self.state.lock();
        // Carry withdrawal/completion across queue handoffs, including UP
        // configurations whose locks provide exclusion without a full fence.
        fence(Ordering::SeqCst);
        state
    }

    /// Accepts a uniquely prepared node without allocating or invoking it.
    ///
    /// # Safety
    ///
    /// The node must satisfy CallbackNode::from_raw's retention and ownership
    /// contract through callback return. Withdraw its protected object before
    /// submission and satisfy independent retirement obligations before execution.
    /// Retain this queue until accepted work finishes. The caller must be in
    /// a context permitting ordinary IRQ-safe locking, never NMI.
    pub(super) unsafe fn submit(&self, node: UnsafeRef<CallbackNode>) {
        let mut state = self.lock();
        state.unassigned.push_back(node);
        // Count accepted work, not just linked nodes: a detached batch retains
        // its completed counts until returning its execution claim.
        state.outstanding += 1;
    }

    /// Appends a marker after all outstanding work without requiring its own GP.
    /// An empty queue returns false without accessing or consuming the marker.
    ///
    /// # Safety
    /// On acceptance, `node` must satisfy CallbackNode::from_raw's unique stable
    /// storage and invocation contract. It must be a completion marker safe to
    /// invoke without a GP of its own, not a normal retirement callback. Prepare
    /// its completion accounting before publication and retain all resources
    /// through invocation. Retain this queue and permit IRQ-safe locking, never
    /// NMI. No callback or notification runs here; the host must request service.
    pub(super) unsafe fn enqueue_drain_marker(&self, node: *mut CallbackNode) -> bool {
        let mut state = self.lock();
        if state.outstanding == 0 {
            return false;
        }
        // SAFETY: acceptance transfers the caller's unique stable marker. Its
        // callback needs only preceding callback completion, not a new GP.
        let marker = unsafe { CallbackNode::from_raw(node) };
        if !state.unassigned.is_empty() {
            state.unassigned.push_back(marker);
        } else {
            // Slots are reused, so array order is not temporal order. Both
            // occupied targets are adjacent; wrapping addition also handles
            // the successor whose numeric value is zero.
            let last = match (state.waiting[0].period, state.waiting[1].period) {
                (Some(first), Some(second)) => {
                    debug_assert!(
                        first.wrapping_add(1) == second || second.wrapping_add(1) == first
                    );
                    Some(usize::from(first.wrapping_add(1) == second))
                }
                (Some(_), None) => Some(0),
                (None, Some(_)) => Some(1),
                (None, None) => None,
            };
            if let Some(index) = last {
                state.waiting[index].callbacks.push_back(marker);
            } else {
                // Outstanding includes detached/invoking work. Its exclusive
                // executor returns any remainder ahead of this marker before
                // releasing the claim, even when eligible was empty here.
                state.eligible.push_back(marker);
            }
        }
        state.outstanding += 1;
        true
    }

    /// Assigns the current finite prefix without hiding it from queue observers.
    /// Returns whether any callbacks were assigned; later submissions stay pending.
    fn assign_pending(&self, period: PeriodId) -> bool {
        let mut state = self.lock();
        if state.unassigned.is_empty() {
            return false;
        }
        let index = state
            .waiting
            .iter()
            .position(|segment| segment.period == Some(period))
            .or_else(|| {
                state
                    .waiting
                    .iter()
                    .position(|segment| segment.period.is_none())
            });
        // A third target would mean the sole driver skipped eager promotion.
        assert!(
            index.is_some(),
            "RCU callback target outside two-period window"
        );
        if let Some(index) = index {
            // Keep capture and assignment under one lock so queue-tail users
            // never mistake a temporarily detached prefix for absent work.
            let prefix = state.unassigned.take();
            let segment = &mut state.waiting[index];
            segment.period = Some(period);
            segment.callbacks.back_mut().splice_after(prefix);
        }
        true
    }

    fn promote(&self, completed: PeriodId) {
        let mut state = self.lock();
        for index in 0..state.waiting.len() {
            if state.waiting[index].period == Some(completed) {
                let ready = state.waiting[index].callbacks.take();
                state.waiting[index].period = None;
                state.eligible.back_mut().splice_after(ready);
            }
        }
    }

    fn needs_period(&self) -> bool {
        self.lock()
            .waiting
            .iter()
            .any(|segment| segment.period.is_some())
    }

    /// Includes assigned, detached and currently invoking callbacks.
    pub(super) fn outstanding(&self) -> usize {
        self.lock().outstanding
    }

    /// Persistent queued readiness; a detached executor may also own work.
    pub(super) fn has_ready(&self) -> bool {
        !self.lock().eligible.is_empty()
    }

    /// Executes at most `budget` callbacks, returning only after queue accounting.
    ///
    /// A competing or recursive executor returns zero rather than waiting.
    /// Unexecuted entries return ahead of any newly eligible work. Readiness
    /// must be rechecked by the host carrier before it relinquishes service.
    ///
    /// # Safety
    ///
    /// The driver must have used valid CPU observations and unique GP control,
    /// not merely simulated quiescence. All independent retirement obligations
    /// must be satisfied. No RCU or callback-dependent lock may be held. Current
    /// context must permit callbacks and their cleanup; neither may block,
    /// unwind or wait for this RCU engine. NMI execution is not supported.
    pub(super) unsafe fn invoke_ready(&self, budget: usize) -> usize {
        if budget == 0 {
            return 0;
        }
        let Some(mut batch) = CallbackBatch::take(self) else {
            return 0;
        };
        for _ in 0..budget {
            let Some(node) = batch.callbacks.pop_front() else {
                break;
            };
            // SAFETY: the exclusive executor unlinked this eligible node;
            // admission retained it, and the caller certifies observation,
            // retirement and execution-context validity. No lock is held.
            unsafe { CallbackNode::invoke(node) };
            // The callback may have freed its node; only batch-owned state is
            // accessed from here onward. Outstanding includes it until return.
            batch.completed += 1;
        }
        let completed = batch.completed;
        drop(batch);
        completed
    }
}

/// Detached work and its exclusive execution claim survive carrier descheduling.
struct CallbackBatch<'queue> {
    queue: &'queue CallbackQueue,
    callbacks: CallbackList,
    completed: usize,
}

impl<'queue> CallbackBatch<'queue> {
    fn take(queue: &'queue CallbackQueue) -> Option<Self> {
        let mut state = queue.lock();
        if state.is_executing || state.eligible.is_empty() {
            return None;
        }
        state.is_executing = true;
        Some(Self {
            queue,
            callbacks: state.eligible.take(),
            completed: 0,
        })
    }
}

impl Drop for CallbackBatch<'_> {
    fn drop(&mut self) {
        let mut state = self.queue.lock();
        state
            .eligible
            .front_mut()
            .splice_before(self.callbacks.take());
        debug_assert!(state.is_executing && state.outstanding >= self.completed);
        state.outstanding -= self.completed;
        state.is_executing = false;
    }
}

/// Sole callback assignment and GP-control owner; not a runtime service task.
///
/// Construct only for an idle coordinator and unassigned/empty queues. No other
/// driver may assign these queues or start/finalize this coordinator. The shared
/// coordinator borrow permits concurrent validated observations/reports, not
/// competing control. Exclusive `&mut self` serializes complete drive passes;
/// retaining a shared coordinator reference alone does not enforce ownership.
pub struct CallbackDriver<'core, 'nodes, const CPUS: usize> {
    coordinator: &'core GracePeriodCoordinator<'nodes, CPUS>,
    queues: &'core [CallbackQueue; CPUS],
    active_period: Option<PeriodId>,
}

impl<'core, 'nodes, const CPUS: usize> CallbackDriver<'core, 'nodes, CPUS> {
    /// Runs one bounded assignment/completion pass for the host service task.
    ///
    /// Returns whether a period remains active and needs observations or a later
    /// progress check. It does not mean the service must busy-loop: the host
    /// should sleep until new demand or its progress deadline. No callback runs
    /// here. Notifications and timer operations belong outside this call.
    pub fn advance(&mut self) -> bool {
        self.drive().is_some()
    }

    /// Creates the sole controller for fresh callback queues.
    ///
    /// # Safety
    /// The coordinator must be idle and all queues empty. The caller must grant
    /// this driver exclusive assignment, promotion and coordinator control for
    /// their remaining lifetime; shared users may only submit, observe, report
    /// and execute eligible callbacks. To hand off control, move this driver,
    /// preserving its active period. Dropping it does not finish a period or
    /// release this obligation: do not reconstruct a driver over the same state.
    pub(super) unsafe fn new(
        coordinator: &'core GracePeriodCoordinator<'nodes, CPUS>,
        queues: &'core [CallbackQueue; CPUS],
    ) -> Self {
        Self {
            coordinator,
            queues,
            active_period: None,
        }
    }

    /// Captures each finite prefix, advances completion and starts required work.
    ///
    /// Does not wait, execute callbacks or infer hardware observations. The
    /// returned current identity is private bookkeeping, not a persistent cookie.
    pub(super) fn drive(&mut self) -> Option<PeriodId> {
        // Only this driver advances completion. Computing the target does not
        // start a period: initialization still follows all prefix captures.
        let target = self.active_period.map_or_else(
            || self.coordinator.completed_period().wrapping_add(1),
            |active| active.wrapping_add(1),
        );
        let mut has_assigned_work = false;
        for queue in self.queues {
            has_assigned_work |= queue.assign_pending(target);
        }
        if let Some(active) = self.active_period {
            let Some(completed) = self.coordinator.try_complete_period() else {
                return Some(active);
            };
            assert_eq!(completed, active, "RCU callback driver lost GP ownership");
            // Persist eligibility everywhere before allowing any new period.
            // No old target remains to be re-compared after an arbitrary delay.
            for queue in self.queues {
                queue.promote(completed);
            }
            self.active_period = None;
        }
        // New assignments target the successor, so promotion cannot erase them.
        // Without new work, a previously assigned successor may still remain.
        if has_assigned_work || self.queues.iter().any(CallbackQueue::needs_period) {
            self.active_period = self.coordinator.begin_period();
            assert!(
                self.active_period.is_some(),
                "RCU coordinator has another driver"
            );
        }
        self.active_period
    }
}

unittest::path_mod_test!(tests, "../ut-tests/callback_queue.rs");

#[cfg(test)]
#[path = "../tests/host/callback_execution.rs"]
mod host_tests;

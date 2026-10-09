// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! A CPU's pending timers and running-timer record; serialized by the caller.

use intrusive_collections::UnsafeRef;

use super::{
    deadline::{Deadline, Expiration},
    index::{TimerIndex, TimerNode},
};

struct RunningTimer {
    // Comparison only; clearing this record ends the core's storage borrowing.
    node_addr: usize,
}

/// Removed returns a borrowed node, never an ownership reference.
pub(super) enum CancelResult {
    Inactive,
    Removed(UnsafeRef<TimerNode>),
    Running,
}

/// One CPU's pending index and running record, sharing one serialization boundary.
///
/// All participating node access must be serialized with this object. A linked
/// node belongs to this index, and storage stays fixed and live through pending,
/// delivery and all external accesses. External adapters keep storage alive;
/// index Drop only unlinks, never releases owners.
pub(super) struct CpuTimers {
    pending: TimerIndex,
    running: Option<RunningTimer>,
    dispatch_after_ns: Option<u64>,
}

// SAFETY: moving the collection moves no linked TimerNode. Unsafe insertion
// requires stable live storage and serialized accesses to every linked node;
// TimerNode is Sync, and all mutations require exclusive collection access.
// The cached pointer follows the same lifetime contract. RunningTimer contains
// only comparison data. SpinNoIrq can therefore transfer access across CPUs.
unsafe impl Send for CpuTimers {}

impl CpuTimers {
    pub(super) const fn new() -> Self {
        Self {
            pending: TimerIndex::new(),
            running: None,
            dispatch_after_ns: None,
        }
    }

    pub(super) fn earliest_deadline_ns(&self) -> Option<u64> {
        self.pending.earliest_deadline_ns()
    }

    pub(super) fn service_deadline_ns(&self) -> Option<u64> {
        self.earliest_deadline_ns()
            .map(|deadline| deadline.max(self.dispatch_after_ns.unwrap_or(0)))
    }

    pub(super) fn can_dispatch(&mut self, now_ns: u64) -> bool {
        if self
            .dispatch_after_ns
            .is_some_and(|deadline| now_ns < deadline)
        {
            return false;
        }
        self.dispatch_after_ns = None;
        self.earliest_deadline_ns()
            .is_some_and(|deadline| deadline <= now_ns)
    }

    pub(super) fn defer_dispatch_until(&mut self, deadline_ns: u64) {
        self.dispatch_after_ns = Some(deadline_ns);
    }

    pub(super) fn is_running(&self, node: &TimerNode) -> bool {
        self.running
            .as_ref()
            .is_some_and(|running| running.node_addr == core::ptr::from_ref(node).addr())
    }

    /// Publishes one deadline, replacing any pending successor.
    /// Returns whether this operation added a new pending membership.
    ///
    /// # Safety
    /// Uphold the CpuTimers storage/serialization contract. A linked node must
    /// belong to THIS pending index. External storage ownership must remain valid
    /// until pending and running membership and all other accesses have ended.
    pub(super) unsafe fn arm(&mut self, node: &TimerNode, deadline: Deadline) -> bool {
        let was_linked = node.is_linked();
        if was_linked {
            // SAFETY: the caller proves membership in this serialized index.
            unsafe { self.pending.remove(node) };
        }
        // SAFETY: removed above if linked; the caller guarantees exclusive
        // node access and stable live storage through subsequent removal.
        unsafe { node.replace_deadline(deadline) };
        // SAFETY: The caller keeps node at this address until removal and all
        // subsequent borrowed accesses end; UnsafeRef takes no storage ownership.
        let pending = unsafe { UnsafeRef::from_raw(node) };
        // SAFETY: node is unlinked, its deadline is prepared, and the caller's
        // serialized access excludes deadline changes throughout membership.
        unsafe { self.pending.insert(pending) };
        !was_linked
    }

    /// Non-waiting cancellation leaves running timers and successors untouched.
    ///
    /// # Safety
    /// Node access is serialized with this CpuTimers and any membership belongs to
    /// this index. Removal does not release storage or authorize freeing it
    /// while other external accesses remain.
    pub(super) unsafe fn try_cancel(&mut self, node: &TimerNode) -> CancelResult {
        if self.is_running(node) {
            return CancelResult::Running;
        }
        if node.is_linked() {
            // SAFETY: the caller proves this exact index owns the linked node.
            CancelResult::Removed(unsafe { self.pending.remove(node) })
        } else {
            CancelResult::Inactive
        }
    }

    /// Takes one due registration only after the preceding delivery has retired.
    /// External ownership keeps the borrowed node live after unlock.
    pub(super) fn take_expired(
        &mut self,
        due_at_ns: u64,
    ) -> Option<(UnsafeRef<TimerNode>, Expiration)> {
        if self.running.is_some() {
            return None;
        }
        let node = self.pending.pop_due(due_at_ns)?;
        // SAFETY: insertion retains storage; the owner lock serializes reads
        // against replacement, and pop_due removed this node from the index.
        let deadline_ns = unsafe { node.deadline() }.deadline_ns();
        let expiration = Expiration { deadline_ns };
        self.running = Some(RunningTimer {
            node_addr: core::ptr::from_ref(node.as_ref()).addr(),
        });
        Some((node, expiration))
    }

    /// Clears only the timers record; never reconstructs a pointer from node_addr.
    ///
    /// # Safety
    /// node_addr must name this delivery. Complete all callback/node accesses,
    /// and callback-resource cleanup BEFORE this call. Keep
    /// external node storage live until this call ends the running borrow;
    /// no delivery node access may follow it.
    pub(super) unsafe fn clear_running(&mut self, node_addr: usize) {
        assert_eq!(self.running.as_ref().map(|r| r.node_addr), Some(node_addr));
        self.running = None;
    }
}

#[unittest::mod_test]
mod tests {
    use ktime_types::{MonotonicInstant, TimeSpan};
    use unittest::{assert, assert_eq, def_test};

    use super::*;

    fn setting(ns: u64) -> Deadline {
        Deadline::new(MonotonicInstant::from_span_since_origin(
            TimeSpan::from_nanos(ns),
        ))
        .unwrap()
    }

    /// # Safety
    /// The caller ended all callback resources and node accesses for this delivery.
    unsafe fn complete(timers: &mut CpuTimers, node: UnsafeRef<TimerNode>) {
        let address = UnsafeRef::into_raw(node).addr();
        // SAFETY: caller finished this delivery; converting the handle ends its borrow.
        unsafe { timers.clear_running(address) };
    }

    #[def_test]
    fn one_shot_consumes_membership_without_implicit_successor() {
        let node = TimerNode::new(setting(10));
        let mut timers = CpuTimers::new();
        // SAFETY: stack-stable node outlives this sequentially accessed collection.
        unsafe { timers.arm(&node, setting(10)) };
        assert!(timers.take_expired(9).is_none());
        let (running, event) = timers.take_expired(35).unwrap();
        assert_eq!(event.deadline_ns, 10);
        assert_eq!(timers.earliest_deadline_ns(), None);
        assert!(timers.take_expired(35).is_none());
        // SAFETY: no callback or borrowed resources remain for this delivery.
        unsafe { complete(&mut timers, running) };
        assert!(timers.take_expired(u64::MAX - 1).is_none());
    }

    #[def_test]
    fn running_replacements_preserve_snapshot_and_cleanup_barrier() {
        let node = TimerNode::new(setting(10));
        let mut timers = CpuTimers::new();
        // SAFETY: stable local storage, all access serialized by this test.
        unsafe { timers.arm(&node, setting(10)) };
        let (running, event) = timers.take_expired(10).unwrap();
        // SAFETY: same running node and collection; successor uses its existing link.
        assert!(unsafe { timers.arm(&node, setting(20)) });
        // SAFETY: The same stack-stable node is accessed only by this test,
        // and its existing membership belongs to timers.
        assert!(!unsafe { timers.arm(&node, setting(30)) });
        // SAFETY: timers owns this node's membership; access remains serialized.
        let canceled = unsafe { timers.try_cancel(&node) };
        assert!(matches!(canceled, CancelResult::Running));
        assert_eq!(event.deadline_ns, 10);
        assert_eq!(timers.earliest_deadline_ns(), Some(30));
        assert!(timers.take_expired(100).is_none());
        // SAFETY: callback resources and delivery references end before publication.
        unsafe { complete(&mut timers, running) };
        let (next, event) = timers.take_expired(100).unwrap();
        assert_eq!(event.deadline_ns, 30);
        // SAFETY: second modeled delivery has no remaining callback resources.
        unsafe { complete(&mut timers, next) };
        assert!(timers.take_expired(100).is_none());
    }

    #[def_test]
    fn cancellation_retries_after_cleanup_with_or_without_successor() {
        for has_successor in [false, true] {
            let node = TimerNode::new(setting(10));
            let mut timers = CpuTimers::new();
            // SAFETY: fixed stack node outlives all sequential tree accesses.
            unsafe { timers.arm(&node, setting(10)) };
            let (running, _) = timers.take_expired(10).unwrap();
            if has_successor {
                // SAFETY: same live running node; no concurrent access.
                unsafe { timers.arm(&node, setting(20)) };
            }
            assert!(matches!(
                // SAFETY: this collection owns the node and current delivery.
                unsafe { timers.try_cancel(&node) },
                CancelResult::Running
            ));
            assert!(timers.take_expired(20).is_none());
            // SAFETY: modeled callback cleanup has ended.
            unsafe { complete(&mut timers, running) };
            // SAFETY: node remains stable and belongs only to this collection.
            match unsafe { timers.try_cancel(&node) } {
                CancelResult::Removed(removed) => {
                    assert!(has_successor);
                    assert!(core::ptr::eq(removed.as_ref(), &node));
                }
                CancelResult::Inactive => assert!(!has_successor),
                CancelResult::Running => assert!(false),
            }
            assert_eq!(timers.earliest_deadline_ns(), None);
        }
    }

    #[def_test]
    fn pending_cancel_removes_only_target_and_idle_can_restart() {
        let nodes = [TimerNode::new(setting(10)), TimerNode::new(setting(10))];
        let mut timers = CpuTimers::new();
        for node in &nodes {
            // SAFETY: fixed array outlives the collection; sequential access only.
            unsafe { timers.arm(node, setting(10)) };
        }
        assert!(matches!(
            // SAFETY: exact member of this collection.
            unsafe { timers.try_cancel(&nodes[0]) },
            CancelResult::Removed(_)
        ));
        let (running, _) = timers.take_expired(10).unwrap();
        assert!(core::ptr::eq(running.as_ref(), &nodes[1]));
        // SAFETY: first modeled delivery has completed, with no retained borrows.
        unsafe { complete(&mut timers, running) };
        // SAFETY: canceled node remains stable and can be reused.
        unsafe { timers.arm(&nodes[0], setting(20)) };
        let (running, event) = timers.take_expired(20).unwrap();
        assert_eq!(event.deadline_ns, 20);
        // SAFETY: no callback resources remain in the second delivery.
        unsafe { complete(&mut timers, running) };
    }

    #[def_test]
    fn backoff_only_changes_service_deadline() {
        let node = TimerNode::new(setting(10));
        let mut timers = CpuTimers::new();
        // SAFETY: stack-stable node outlives this sequential collection.
        unsafe { timers.arm(&node, setting(10)) };
        timers.defer_dispatch_until(50);
        assert_eq!(timers.earliest_deadline_ns(), Some(10));
        assert_eq!(timers.service_deadline_ns(), Some(50));
        assert!(!timers.can_dispatch(49));
        assert!(timers.can_dispatch(50));
        assert_eq!(timers.service_deadline_ns(), Some(10));
        // SAFETY: still-live member of this collection, not running.
        unsafe { timers.try_cancel(&node) };
        assert_eq!(timers.service_deadline_ns(), None);
    }

    #[def_test]
    fn external_storage_is_released_after_running_clear() {
        let node = alloc::boxed::Box::new(TimerNode::new(setting(10)));
        let mut timers = CpuTimers::new();
        // SAFETY: allocation remains live through running retirement.
        unsafe { timers.arm(&node, setting(10)) };
        let (running, _) = timers.take_expired(10).unwrap();
        assert!(timers.take_expired(20).is_none());
        // SAFETY: no callback resources remain and owner storage is still live.
        unsafe { complete(&mut timers, running) };
        drop(node);
        assert!(timers.running.is_none());
        assert!(timers.take_expired(20).is_none());
    }
}

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

#[cfg(unittest)]
unittest::path_mod_test!(tests, "../ut-tests/cpu_unittest.rs");

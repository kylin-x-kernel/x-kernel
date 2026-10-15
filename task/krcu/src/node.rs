// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Original-CPU ownership of preempted task readers.

use core::sync::atomic::{AtomicBool, Ordering, fence};

use intrusive_collections::{LinkedList, UnsafeRef, intrusive_adapter};
use kspin::SpinNoIrq;

use crate::{PeriodId, TaskReader};

intrusive_adapter!(ReaderAdapter = UnsafeRef<TaskReader>: TaskReader {
    link => intrusive_collections::LinkedListAtomicLink
});

/// A CPU's enrolled readers, serialized independently of every other CPU.
///
/// The list borrows embedded task storage; it does not allocate or own tasks.
/// Migration retains the original node until the outer scope exits.
pub struct ReaderNode {
    state: SpinNoIrq<NodeState>,
    // Writes require the node lock. An unlocked false only skips observation;
    // it never certifies quiescence or permits skipping reader enrollment.
    is_cpu_pending: AtomicBool,
}

struct NodeState {
    readers: LinkedList<ReaderAdapter>,
    period: Option<NodePeriod>,
}

/// A period waits for the CPU observation and the suffix starting at this reader.
/// The marker borrows list-owned storage under the same node lock; it adds no
/// task reference and must advance before the marked reader can be reclaimed.
struct NodePeriod {
    id: PeriodId,
    first_blocker: Option<UnsafeRef<TaskReader>>,
}

impl ReaderNode {
    /// Creates an empty node. Keep it stable while it has enrolled readers.
    pub const fn new() -> Self {
        Self {
            state: SpinNoIrq::new(NodeState {
                readers: LinkedList::new(ReaderAdapter::NEW),
                period: None,
            }),
            is_cpu_pending: AtomicBool::new(false),
        }
    }

    /// Records the outgoing task before the CPU can stop representing its read.
    /// Serial repeated calls, including after task migration, preserve existing
    /// membership. This does not support concurrent enrollment of one reader.
    ///
    /// # Safety
    ///
    /// Called by the scheduler for its outgoing current task, with IRQs and
    /// preemption excluded; never from NMI. No other scheduler may concurrently
    /// enroll this reader. Keep this node and reader at stable addresses through
    /// removal and all subsequent accesses. Before reclaiming a task that
    /// abandons an active scope, the host must call [`TaskReader::finish_task_exit`].
    /// Only that task may enter/exit its scopes; interrupt-context scopes are
    /// not supported by this enrollment implementation.
    pub unsafe fn enroll(&self, reader: &TaskReader) {
        if !reader.is_reading() || reader.is_enrolled() {
            return;
        }
        let mut state = self.state.lock();
        // The owner-node lock orders enrollment with future GP observations,
        // including when the underlying UP lock is only an exclusion guard.
        fence(Ordering::SeqCst);
        // SAFETY: the exclusive scheduler caller checked that this reader
        // needs enrollment, and the node lock protects its destination list.
        unsafe { self.enroll_locked(reader, &mut state) };
    }

    /// # Safety
    /// The caller holds this node's state lock and satisfies `enroll`'s storage
    /// and scheduler-exclusion contract. The reader is not enrolled anywhere.
    unsafe fn enroll_locked(&self, reader: &TaskReader, state: &mut NodeState) {
        // SAFETY: the scheduler contract keeps embedded storage fixed and live
        // through removal. This outgoing reader is not in any other node.
        let borrowed = unsafe { UnsafeRef::from_raw(reader) };
        let NodeState { readers, period } = state;
        if let Some(period) = period
            .as_mut()
            .filter(|_| self.is_cpu_pending.load(Ordering::Relaxed))
        {
            // Before the CPU observation, enrollment must preserve protection
            // for this period. Append to the blocker suffix, never ahead of it.
            if period.first_blocker.is_none() {
                period.first_blocker = Some(borrowed.clone());
            }
            readers.push_back(borrowed);
        } else {
            // A fresh reader after the observation belongs only to future
            // periods. Inserting ahead of the suffix cannot extend this one.
            readers.push_front(borrowed);
        }
        reader
            .owner
            .store(core::ptr::from_ref(self).cast_mut(), Ordering::Release);
    }

    /// # Safety
    /// The reader belongs to this node, which remains live and fixed. Only the
    /// task's outer exit or exclusive terminal cleanup may remove it; no
    /// concurrent enrollment/removal is allowed. Normal IRQ/preemption exclusion
    /// is acquired here.
    pub(super) unsafe fn remove(&self, reader: &TaskReader) {
        let mut state = self.state.lock();
        fence(Ordering::SeqCst);
        debug_assert!(
            core::ptr::eq(reader.owner.load(Ordering::Acquire), self),
            "RCU reader does not belong to this node"
        );
        let NodeState { readers, period } = &mut *state;
        // SAFETY: owner publication identifies this exact list; the caller is
        // the sole remover and this lock serializes all list mutations.
        let mut cursor = unsafe { readers.cursor_mut_from_ptr(reader) };
        let removed = cursor.remove();
        assert!(
            removed.is_some(),
            "enrolled RCU reader missing from owner node"
        );
        if let Some(period) = period
            && period
                .first_blocker
                .as_ref()
                .is_some_and(|first| core::ptr::eq(first.as_ref(), reader))
        {
            // remove advances the cursor to the next list member. Any
            // non-blocking readers are before this suffix, never after it.
            period.first_blocker = cursor.as_cursor().clone_pointer();
        }
        reader.owner.store(core::ptr::null_mut(), Ordering::Release);
    }

    /// Number of retained readers for isolated node and scheduler tests.
    #[cfg(any(test, unittest))]
    pub fn reader_count(&self) -> usize {
        self.state.lock().readers.iter().count()
    }
}

impl ReaderNode {
    pub(crate) fn needs_cpu_observation(&self) -> bool {
        self.is_cpu_pending.load(Ordering::Acquire)
    }

    /// Includes every existing reader and starts owing a CPU observation.
    /// An unfinished period cannot be overwritten, even by a duplicate start.
    pub(crate) fn begin_period(&self, id: PeriodId) -> bool {
        let mut state = self.state.lock();
        fence(Ordering::SeqCst);
        if state.period.is_some() {
            return false;
        }
        state.period = Some(NodePeriod {
            id,
            first_blocker: state.readers.front().clone_pointer(),
        });
        self.is_cpu_pending.store(true, Ordering::Release);
        true
    }

    /// Records an externally validated CPU observation for this exact period.
    /// This bookkeeping operation does not itself prove a quiescent boundary.
    pub(crate) fn observe_cpu(&self, id: PeriodId) -> bool {
        let state = self.state.lock();
        fence(Ordering::SeqCst);
        let Some(_) = state.period.as_ref().filter(|period| period.id == id) else {
            return false;
        };
        self.is_cpu_pending.store(false, Ordering::Release);
        true
    }

    /// Enrolls the outgoing reader and records a needed scheduler observation.
    /// Returns a ready identity only for immediate reporting within that same
    /// excluded interval, never for storage in deferred work.
    ///
    /// # Safety
    /// The caller satisfies `enroll`'s scheduler and storage contract, and keeps
    /// IRQs/preemption excluded through immediate coordinator acceptance.
    pub(crate) unsafe fn note_context_switch(&self, reader: &TaskReader) -> Option<PeriodId> {
        let needs_enrollment = reader.is_reading() && !reader.is_enrolled();
        if !needs_enrollment && !self.is_cpu_pending.load(Ordering::Acquire) {
            // A racing begin may be missed here, but its obligation stays set.
            // The GP service retries observation assistance; no report is made.
            return None;
        }
        let mut state = self.state.lock();
        fence(Ordering::SeqCst);
        if needs_enrollment {
            // SAFETY: the outgoing reader cannot enter/exit or be enrolled by
            // another scheduler, and this lock protects its destination list.
            unsafe { self.enroll_locked(reader, &mut state) };
        }
        // Revalidate under the lock: the unlocked hint identifies no period.
        // After CPU observation, old-reader completion is reported by the GP
        // service's readiness scan, not by repeatedly reporting each switch.
        if !self.is_cpu_pending.load(Ordering::Relaxed) {
            return None;
        }
        let period = state.period.as_ref()?;
        self.is_cpu_pending.store(false, Ordering::Release);
        period.first_blocker.is_none().then_some(period.id)
    }

    /// Snapshots a locally ready period; it is not a global completion report.
    /// Numeric identities must not be retained across an unbounded scheduling
    /// gap or a wrap. Deferred service must inspect the node afresh.
    pub(crate) fn ready_period(&self) -> Option<PeriodId> {
        let state = self.state.lock();
        fence(Ordering::SeqCst);
        state
            .period
            .as_ref()
            .filter(|period| {
                !self.is_cpu_pending.load(Ordering::Relaxed) && period.first_blocker.is_none()
            })
            .map(|period| period.id)
    }

    /// Releases a matching ready period, preserving readers for the next one.
    pub(crate) fn finish_period(&self, id: PeriodId) -> bool {
        let mut state = self.state.lock();
        fence(Ordering::SeqCst);
        if !state.period.as_ref().is_some_and(|period| {
            period.id == id
                && !self.is_cpu_pending.load(Ordering::Relaxed)
                && period.first_blocker.is_none()
        }) {
            return false;
        }
        state.period = None;
        true
    }
}

#[cfg(any(test, unittest))]
#[path = "../ut-tests/node.rs"]
mod tests;

impl Default for ReaderNode {
    fn default() -> Self {
        Self::new()
    }
}

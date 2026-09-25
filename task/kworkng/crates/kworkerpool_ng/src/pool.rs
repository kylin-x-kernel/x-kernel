// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Normal service worker capacity, adapted from the production worker pool.
//!
//! This is owner-local state, not a running pool or wake protocol. The normal
//! service protects it together with source/availability decisions. No task
//! creation, activation, wake, park, destruction or callback is performed here.

use alloc::{boxed::Box, vec};
use core::num::NonZeroUsize;

use ktime_types::{MonotonicInstant, TimeSpan};

use crate::worker::WorkerState;

/// One CPU's fixed worker-slot budget, shared by that CPU's ordinary queues.
/// Slots are prepared once. Creating through Retiring all consume capacity;
/// only confirmed disposal/exit releases it. Slot indices are private owner-local
/// identities, not durable handles for asynchronously retained commands.
pub(crate) struct WorkerPool {
    workers: Box<[WorkerState]>,
    reserved_workers: usize,
}

impl WorkerPool {
    pub(crate) fn new(limit: NonZeroUsize) -> Self {
        Self {
            workers: vec![WorkerState::Empty; limit.get()].into_boxed_slice(),
            reserved_workers: 0,
        }
    }

    #[cfg(unittest)]
    pub(crate) fn reserved_workers(&self) -> usize {
        self.reserved_workers
    }

    pub(crate) fn state(&self, worker: usize) -> WorkerState {
        self.workers[worker]
    }

    pub(crate) fn has_retiring_workers(&self) -> bool {
        self.workers.contains(&WorkerState::Retiring)
    }

    fn has_excess_idle_workers(&self) -> bool {
        self.workers
            .iter()
            .filter(|state| state.is_idle())
            .take(2)
            .count()
            == 2
    }

    /// Earliest policy deadline when no source demand exists. The same baseline
    /// predicate is used by request_retirement; this does not authorize retirement.
    /// Deadlines beyond timer range are omitted, never clamped to an earlier time.
    /// Such workers remain available for demand; later activity can recheck age.
    pub(crate) fn next_idle_deadline(&self, idle_for: TimeSpan) -> Option<MonotonicInstant> {
        assert!(
            idle_for > TimeSpan::ZERO,
            "idle retirement requires a positive interval"
        );
        if !self.has_excess_idle_workers() {
            return None;
        }
        self.workers
            .iter()
            .filter_map(|state| match state {
                WorkerState::Parked { since } => since
                    .checked_add(idle_for)
                    .filter(|deadline| deadline.span_since_origin().try_as_nanos_u64().is_some()),
                _ => None,
            })
            .min()
    }

    /// Shared growth predicate for source notification and manager reservation.
    /// A startup/selection context can still serve demand; Occupied cannot.
    pub(crate) fn needs_worker(&self, has_demand: bool) -> bool {
        if self.reserved_workers == self.workers.len() {
            return false;
        }
        self.reserved_workers == 0
            || (has_demand
                && !self.workers.iter().any(|state| {
                    matches!(
                        state,
                        WorkerState::Creating
                            | WorkerState::Prepared
                            | WorkerState::Available
                            | WorkerState::Selecting
                            | WorkerState::Parked { .. }
                    )
                }))
    }

    /// Manager-only search; reserve before releasing protection to prepare a task.
    /// Saturation has no effects and does not discard any source demand.
    pub(crate) fn reserve_worker(&mut self) -> Option<usize> {
        if self.reserved_workers == self.workers.len() {
            return None;
        }
        let worker = self
            .workers
            .iter()
            .position(|state| *state == WorkerState::Empty)
            .expect("reserved capacity matches nonempty slots");
        self.workers[worker] = WorkerState::Creating;
        self.reserved_workers += 1;
        Some(worker)
    }

    /// Runtime has prepared task/wake resources but has not activated the task.
    pub(crate) fn worker_prepared(&mut self, worker: usize) {
        self.transition(worker, WorkerState::Creating, WorkerState::Prepared);
    }

    /// Called by the started worker after installing its task-bound wait state.
    /// Activation may not yet have returned to the manager; it must not publish
    /// another phase after activation that overwrites this worker-owned progress.
    pub(crate) fn worker_started(&mut self, worker: usize) {
        self.transition(worker, WorkerState::Prepared, WorkerState::Available);
    }

    /// Only an available worker may inspect management; no callback runs here.
    pub(crate) fn begin_selection(&mut self, worker: usize) -> bool {
        if self.workers[worker] != WorkerState::Available {
            return false;
        }
        self.workers[worker] = WorkerState::Selecting;
        true
    }

    /// Source inspection has returned, without invoking the acquired callback.
    pub(crate) fn finish_selection(&mut self, worker: usize, has_execution: bool) {
        let next = if has_execution {
            WorkerState::Occupied
        } else {
            WorkerState::Available
        };
        self.transition(worker, WorkerState::Selecting, next);
    }

    /// Call only after AcquiredExecution::run returns, including resource cleanup.
    /// Blocking/resuming inside that execution does not change this state.
    pub(crate) fn finish_execution(&mut self, worker: usize) {
        self.transition(worker, WorkerState::Occupied, WorkerState::Available);
    }

    /// Caller checked no demand under the same service protection. This publishes
    /// intent only; release protection before the task's durable-latch park.
    pub(crate) fn park_worker(&mut self, worker: usize, now: MonotonicInstant) {
        self.transition(
            worker,
            WorkerState::Available,
            WorkerState::Parked { since: now },
        );
    }

    /// Reserve a parked worker for demand before delivering its wake unlocked.
    /// Retiring and occupied workers never become claimable through notification.
    pub(crate) fn wake_worker(&mut self, worker: usize) -> bool {
        if !matches!(self.workers[worker], WorkerState::Parked { .. }) {
            return false;
        }
        self.workers[worker] = WorkerState::Available;
        true
    }

    /// Manager-only idle-policy decision. The owner must first reconcile source
    /// demand; an available/parked baseline is kept even if all other workers are
    /// occupied. No task exits and no capacity is returned by this operation.
    ///
    /// Panics if the internal idle interval is zero.
    pub(crate) fn request_retirement(
        &mut self,
        worker: usize,
        now: MonotonicInstant,
        idle_for: TimeSpan,
    ) -> bool {
        assert!(
            idle_for > TimeSpan::ZERO,
            "idle retirement requires a positive interval"
        );
        let WorkerState::Parked { since } = self.workers[worker] else {
            return false;
        };
        if now
            .checked_duration_since(since)
            .is_none_or(|age| age < idle_for)
        {
            return false;
        }
        if !self.has_excess_idle_workers() {
            return false;
        }
        self.retire_parked_worker(worker);
        true
    }

    /// Caller established idle policy or whole-service quiescent teardown under
    /// source protection. Queue shutdown must never use this baseline bypass.
    pub(crate) fn retire_parked_worker(&mut self, worker: usize) {
        assert!(matches!(self.workers[worker], WorkerState::Parked { .. }));
        self.workers[worker] = WorkerState::Retiring;
    }

    /// Runtime has disposed all resources for an attempt that was never activated.
    /// The manager must not have any late completion still referring to this slot.
    #[cfg(unittest)]
    pub(crate) fn release_unstarted_worker(&mut self, worker: usize) {
        assert!(
            matches!(
                self.workers[worker],
                WorkerState::Creating | WorkerState::Prepared
            ),
            "only unactivated preparation may be disposed"
        );
        self.release(worker);
    }

    /// Runtime has confirmed task exit and released its owning task reference.
    /// No late runtime command may use this index after its capacity is returned.
    pub(crate) fn worker_exited(&mut self, worker: usize) {
        assert_eq!(self.workers[worker], WorkerState::Retiring);
        self.release(worker);
    }

    fn transition(&mut self, worker: usize, previous: WorkerState, next: WorkerState) {
        assert_eq!(
            self.workers[worker], previous,
            "unexpected worker lifecycle transition"
        );
        self.workers[worker] = next;
    }

    fn release(&mut self, worker: usize) {
        self.workers[worker] = WorkerState::Empty;
        self.reserved_workers = self
            .reserved_workers
            .checked_sub(1)
            .expect("a released worker must own capacity");
    }
}

#[cfg(unittest)]
mod tests;

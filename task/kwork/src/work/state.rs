// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use crate::{QueueWorkResult, WorkQueuePoolBinding, WorkerExecutionToken, WorkerId};

/// Coarse work lifecycle state.
///
/// The state payload lives in `WorkStateKind`; this enum is intentionally only
/// the public-to-this-module discriminator used by diagnostics, tests, and
/// operation dispatch.
///
/// Allowed transitions:
///
/// ```text
/// Idle
///   | queue_work
///   v
/// Pending -------- take_runnable --------> Running
///   ^                                      | finish
///   | delayed timer fires / flush          v
/// DelayedPending <---- queue_delayed ---- Idle
///   | cancel / stale timer
///   v
/// Idle
///
/// Running -- cancel_sync -----------------> Running
/// Running [cancel gate set] -- finish current run -> Idle
/// ```
///
/// Any other transition is rejected by the operation layer or treated as a
/// stale queue entry. This keeps delayed reservation, pending queue ownership,
/// and running worker ownership tied to the state that actually uses them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WorkStatus {
    Idle,
    DelayedPending,
    Pending,
    Running,
}

pub(crate) enum RunQueueEntryClaim {
    Run {
        binding: WorkQueuePoolBinding,
        instance_id: WorkInstanceId,
    },
    Stale(WorkStatus),
}

/// Identity of one queued or reserved instance of a [`ScheduledWork`].
///
/// A work item may be queued again after a later cancel or teardown cycle. The
/// instance id lets flush and delayed timer paths distinguish the instance they
/// observed from a later queued instance of the same `ScheduledWork`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WorkInstanceId(u64);

impl WorkInstanceId {
    const FIRST: Self = Self(1);

    fn next(self) -> Self {
        Self(self.0.wrapping_add(1).max(1))
    }

    #[cfg(unittest)]
    pub(crate) const fn for_tests(id: u64) -> Self {
        Self(id)
    }
}

/// Timer-owned delayed reservation which has not reached a pool worklist.
///
/// Linux reference: `__queue_delayed_work()` stores the target `wq/cpu` in
/// `struct delayed_work` and arms the timer without inserting
/// `work_struct->entry` into a `pool_workqueue`. `target_pool_key` is therefore
/// only the future pool used by timer fire / flush-delayed immediate queueing;
/// it is not current pool ownership.
#[derive(Clone)]
struct DelayedWorkReservation {
    id: WorkInstanceId,
    target_pool_key: usize,
}

/// Pool-worklist-owned queued instance.
///
/// Linux reference: after `__queue_work()`, `work_struct->data` points at the
/// owning `pool_workqueue` and carries the color used by `nr_in_flight[]`.
/// These fields exist only for entries that are actually pending on a pool
/// worklist or have been claimed by a worker from that worklist.
#[derive(Clone)]
struct QueuedWorkInstance {
    id: WorkInstanceId,
    binding: WorkQueuePoolBinding,
    color: WorkColor,
}

#[derive(Clone)]
struct RunningWorkInstance {
    queued: QueuedWorkInstance,
    worker_id: Option<WorkerId>,
    worker_token: WorkerExecutionToken,
    is_canceling: bool,
}

#[derive(Clone)]
enum WorkStateKind {
    Idle,
    DelayedPending(DelayedWorkReservation),
    Pending(QueuedWorkInstance),
    Running(RunningWorkInstance),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WorkColor(u8);

impl WorkColor {
    /// Number of workqueue flush colors.
    ///
    /// Linux uses `WORK_NR_COLORS == 16` so concurrent flushers can reserve
    /// distinct snapshot colors before overflow/cascade handling is needed.
    /// X-Kernel keeps the same color space even while the full flusher relay
    /// machinery is still being completed.
    pub(crate) const COUNT: usize = 16;
    pub(crate) const DEFAULT: Self = Self(0);

    pub(crate) fn next(self) -> Self {
        let next = (usize::from(self.0) + 1) % Self::COUNT;
        Self(next as u8)
    }

    pub(crate) fn index(self) -> usize {
        usize::from(self.0)
    }
}

/// Per-work lifecycle state.
///
/// `next_instance_id` is a per-`ScheduledWork` queued-instance allocator. It is not
/// an execution count: a reserved delayed instance or a canceled pending
/// instance consumes an id even if the callback never runs.
///
/// This is the current Rust representation of the Linux `work_struct->data`
/// ownership boundary. Today it carries the state discriminator plus pending
/// owner/pool/color payload through `WorkStateKind`. Flush barriers belong to
/// the concrete queued `WorkEntry` or running worker slot, matching Linux's
/// list-position model instead of storing pre-pool delayed barriers here.
pub(crate) struct WorkState {
    next_instance_id: WorkInstanceId,
    kind: WorkStateKind,
}

impl WorkState {
    pub(crate) const fn new() -> Self {
        Self {
            next_instance_id: WorkInstanceId::FIRST,
            kind: WorkStateKind::Idle,
        }
    }

    pub(crate) fn status(&self) -> WorkStatus {
        match &self.kind {
            WorkStateKind::Idle => WorkStatus::Idle,
            WorkStateKind::DelayedPending(_) => WorkStatus::DelayedPending,
            WorkStateKind::Pending(_) => WorkStatus::Pending,
            WorkStateKind::Running(_) => WorkStatus::Running,
        }
    }

    pub(crate) fn allocate_instance_id(&mut self) -> WorkInstanceId {
        let id = self.next_instance_id;
        self.next_instance_id = self.next_instance_id.next();
        id
    }

    pub(crate) fn pending_instance_id(&self) -> Option<WorkInstanceId> {
        match &self.kind {
            WorkStateKind::DelayedPending(pending) => Some(pending.id),
            WorkStateKind::Pending(pending) => Some(pending.id),
            WorkStateKind::Idle | WorkStateKind::Running(_) => None,
        }
    }

    pub(crate) fn pending_binding(&self) -> Option<&WorkQueuePoolBinding> {
        match &self.kind {
            WorkStateKind::Pending(pending) => Some(&pending.binding),
            WorkStateKind::Idle | WorkStateKind::DelayedPending(_) | WorkStateKind::Running(_) => {
                None
            }
        }
    }

    pub(crate) fn pending_binding_cloned(&self) -> Option<WorkQueuePoolBinding> {
        self.pending_binding().cloned()
    }

    pub(crate) fn pending_pool_key(&self) -> usize {
        match &self.kind {
            WorkStateKind::DelayedPending(pending) => pending.target_pool_key,
            WorkStateKind::Pending(pending) => pending.binding.pool_key(),
            WorkStateKind::Idle | WorkStateKind::Running(_) => 0,
        }
    }

    pub(crate) fn pending_color(&self) -> WorkColor {
        match &self.kind {
            WorkStateKind::Pending(pending) => pending.color,
            WorkStateKind::Idle | WorkStateKind::DelayedPending(_) | WorkStateKind::Running(_) => {
                WorkColor::DEFAULT
            }
        }
    }

    pub(crate) fn running_instance_id(&self) -> Option<WorkInstanceId> {
        self.running().map(|running| running.queued.id)
    }

    pub(crate) fn running_binding_cloned(&self) -> Option<WorkQueuePoolBinding> {
        self.running().map(|running| running.queued.binding.clone())
    }

    pub(crate) fn running_pool_key(&self) -> usize {
        self.running()
            .map_or(0, |running| running.queued.binding.pool_key())
    }

    pub(crate) fn running_color(&self) -> WorkColor {
        self.running()
            .map_or(WorkColor::DEFAULT, |running| running.queued.color)
    }

    pub(crate) fn running_worker_id(&self) -> Option<WorkerId> {
        self.running().and_then(|running| running.worker_id)
    }

    pub(crate) fn running_worker_token(&self) -> Option<WorkerExecutionToken> {
        self.running().map(|running| running.worker_token)
    }

    pub(crate) fn running_is_canceling(&self) -> bool {
        self.running().is_some_and(|running| running.is_canceling)
    }

    pub(crate) fn pending_binding_is(&self, binding: &WorkQueuePoolBinding) -> bool {
        self.pending_binding().is_some_and(|pending_binding| {
            pending_binding.pool_key() == binding.pool_key()
                && pending_binding.owner().same_queue(binding.owner().queue())
        })
    }

    pub(crate) fn set_pending(
        &mut self,
        id: WorkInstanceId,
        binding: WorkQueuePoolBinding,
        color: WorkColor,
    ) {
        self.kind = WorkStateKind::Pending(QueuedWorkInstance { id, binding, color });
    }

    pub(crate) fn set_delayed_pending(&mut self, id: WorkInstanceId, pool_key: usize) {
        self.kind = WorkStateKind::DelayedPending(DelayedWorkReservation {
            id,
            target_pool_key: pool_key,
        });
    }

    pub(crate) fn set_running(
        &mut self,
        id: WorkInstanceId,
        binding: WorkQueuePoolBinding,
        worker_id: WorkerId,
        worker_token: WorkerExecutionToken,
        color: WorkColor,
    ) {
        self.kind = WorkStateKind::Running(RunningWorkInstance {
            queued: QueuedWorkInstance { id, binding, color },
            worker_id: Some(worker_id),
            worker_token,
            is_canceling: false,
        });
    }

    pub(crate) fn take_pending_for_run(
        &mut self,
        binding: WorkQueuePoolBinding,
        worker_id: WorkerId,
        worker_token: WorkerExecutionToken,
    ) -> WorkInstanceId {
        let id = self
            .pending_instance_id()
            .expect("running a work item requires a pending instance");
        let color = self.pending_color();
        self.set_running(id, binding, worker_id, worker_token, color);
        id
    }

    pub(crate) fn claim_pool_entry_for_run(
        &mut self,
        pool_key: usize,
        binding_key: usize,
        instance_id: WorkInstanceId,
        worker_id: WorkerId,
        worker_token: WorkerExecutionToken,
    ) -> RunQueueEntryClaim {
        let Some(binding) = self.pending_binding_cloned() else {
            return RunQueueEntryClaim::Stale(self.status());
        };
        let is_same_pool_entry = matches!(self.status(), WorkStatus::Pending)
            && self.pending_pool_key() == pool_key
            && binding.binding_key() == binding_key
            && self.pending_instance_id() == Some(instance_id);
        if !is_same_pool_entry {
            return RunQueueEntryClaim::Stale(self.status());
        }

        let instance_id = self.take_pending_for_run(binding.clone(), worker_id, worker_token);
        RunQueueEntryClaim::Run {
            binding,
            instance_id,
        }
    }

    pub(crate) fn can_queue_now(&self) -> Result<(), QueueWorkResult> {
        match self.status() {
            WorkStatus::Idle => Ok(()),
            WorkStatus::Running => {
                if self.running_is_canceling() {
                    Err(QueueWorkResult::Disabled)
                } else {
                    Err(QueueWorkResult::AlreadyQueued)
                }
            }
            WorkStatus::Pending | WorkStatus::DelayedPending => Err(QueueWorkResult::AlreadyQueued),
        }
    }

    pub(crate) fn cancel_running(&mut self) {
        if let Some(running) = self.running_mut() {
            running.is_canceling = true;
        }
    }

    pub(crate) fn set_idle(&mut self) {
        self.kind = WorkStateKind::Idle;
    }

    fn running(&self) -> Option<&RunningWorkInstance> {
        match &self.kind {
            WorkStateKind::Running(running) => Some(running),
            WorkStateKind::Idle | WorkStateKind::DelayedPending(_) | WorkStateKind::Pending(_) => {
                None
            }
        }
    }

    fn running_mut(&mut self) -> Option<&mut RunningWorkInstance> {
        match &mut self.kind {
            WorkStateKind::Running(running) => Some(running),
            WorkStateKind::Idle | WorkStateKind::DelayedPending(_) | WorkStateKind::Pending(_) => {
                None
            }
        }
    }
}

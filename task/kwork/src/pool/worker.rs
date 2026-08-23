// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use ktime_types::{MonotonicInstant, TimeSpan};

use crate::{BarrierAttachResult, WorkBarrier, WorkBarrierQueue, WorkInstanceId};

/// Opaque worker identity inside one execution pool.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkerId(usize);

impl WorkerId {
    /// Creates a bounded worker identity.
    pub const fn new(id: usize) -> Self {
        Self(id)
    }

    /// Returns the numeric worker id.
    pub const fn as_usize(self) -> usize {
        self.0
    }
}

/// Pool-local identity of one worker-slot execution.
///
/// Linux keeps the currently executing work in `worker->current_work` and uses
/// worker flags under `pool->lock` to make scheduler callbacks operate on the
/// live execution. X-Kernel additionally carries this generation through
/// task-local scheduler context so a stale finish/tick/sleep notification from
/// an older use of the same worker slot cannot update a later execution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkerExecutionToken(usize);

impl WorkerExecutionToken {
    const FIRST: Self = Self(1);

    fn next(self) -> Self {
        Self(self.0.wrapping_add(1).max(1))
    }

    /// Creates a worker execution token from provider-owned task-local state.
    pub const fn from_usize(token: usize) -> Self {
        Self(token)
    }

    /// Returns the numeric token stored by the task provider.
    pub const fn as_usize(self) -> usize {
        self.0
    }
}

/// Lifecycle state of one worker record inside a [`super::WorkerPool`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WorkerState {
    /// No provider task is associated with this worker record.
    Empty,
    /// A manager has reserved this record while creating the provider task.
    Creating,
    /// The provider task exists and is waiting for work.
    Idle,
    /// The worker has been selected for wakeup but has not taken work yet.
    Preparing,
    /// The worker is executing a work callback and participates in concurrency accounting.
    Running,
    /// The worker is executing a callback but is blocked in a sleepable wait.
    Sleeping,
}

/// Worker execution record owned by a worker pool.
///
/// This is the X-Kernel counterpart of the core scheduling fields of Linux
/// `struct worker`: the actual task is provider-owned, while `kwork` records
/// the current work instance and barriers needed by flush/cancel.
pub(crate) struct Worker {
    pub(crate) state: WorkerState,
    pub(crate) generation: WorkerExecutionToken,
    pub(crate) current_work_key: usize,
    pub(crate) current_instance_id: Option<WorkInstanceId>,
    /// Marks the current execution as CPU-intensive for concurrency accounting.
    ///
    /// This is the X-Kernel counterpart of Linux `WORKER_CPU_INTENSIVE`. A
    /// marked worker does not count toward the pool's `nr_running`, so queued
    /// work can wake another worker instead of waiting behind a CPU-bound
    /// callback. The flag is per-execution: it is always cleared when the
    /// current work finishes, matching Linux `worker_clr_flags`.
    pub(crate) cpu_intensive: bool,
    /// Monotonic time when the current work execution started.
    ///
    /// The pool compares this against its CPU-intensive threshold to decide
    /// when a running worker stops counting toward concurrency.
    pub(crate) current_run_started: Option<MonotonicInstant>,
    /// Barriers scheduled behind the current running work.
    ///
    /// This is the X-Kernel counterpart of Linux `worker->scheduled` for
    /// `wq_barrier` linked to a running work. It is bounded so
    /// `flush_work()` cannot grow storage while holding pool state.
    running_barriers: WorkBarrierQueue,
}

impl Worker {
    pub(crate) const fn new() -> Self {
        Self {
            state: WorkerState::Empty,
            generation: WorkerExecutionToken::FIRST,
            current_work_key: 0,
            current_instance_id: None,
            cpu_intensive: false,
            current_run_started: None,
            running_barriers: WorkBarrierQueue::new(),
        }
    }

    pub(crate) fn is_running_instance(&self, work_key: usize, instance_id: WorkInstanceId) -> bool {
        self.current_work_key == work_key && self.current_instance_id == Some(instance_id)
    }

    pub(crate) fn is_current_execution(
        &self,
        work_key: usize,
        instance_id: WorkInstanceId,
        token: WorkerExecutionToken,
    ) -> bool {
        self.generation == token && self.is_running_instance(work_key, instance_id)
    }

    pub(crate) fn next_execution_token(&mut self) -> WorkerExecutionToken {
        self.generation = self.generation.next();
        self.generation
    }

    pub(crate) fn clear_current_work(&mut self) -> alloc::vec::Vec<WorkBarrier> {
        self.current_work_key = 0;
        self.current_instance_id = None;
        self.cpu_intensive = false;
        self.current_run_started = None;
        self.running_barriers.take_all()
    }

    pub(crate) fn set_current_work(
        &mut self,
        work_key: usize,
        instance_id: WorkInstanceId,
        barriers: alloc::vec::Vec<WorkBarrier>,
        run_started: MonotonicInstant,
    ) {
        self.current_work_key = work_key;
        self.current_instance_id = Some(instance_id);
        self.cpu_intensive = false;
        self.current_run_started = Some(run_started);
        let stale = self.running_barriers.take_all();
        debug_assert!(stale.is_empty());
        let appended = self.running_barriers.append_from_vec(barriers);
        debug_assert!(appended);
    }

    /// Returns whether the current execution ran for at least `threshold`.
    pub(crate) fn run_exceeds(&self, threshold: TimeSpan, now: MonotonicInstant) -> bool {
        self.current_run_started
            .is_some_and(|started| now.saturating_duration_since(started) >= threshold)
    }

    pub(crate) fn push_running_barrier(&mut self, barrier: WorkBarrier) -> BarrierAttachResult {
        if self.running_barriers.push_front_linked_barrier(barrier) {
            BarrierAttachResult::Attached
        } else {
            BarrierAttachResult::Full
        }
    }
}

/// Pool-side accounting result of one worker block notification.
pub(crate) struct WorkerSleepTransition {
    /// Whether the worker really transitioned Running → Sleeping.
    pub(crate) did_sleep: bool,
    /// Wake targets chosen by kick evaluation; empty unless `did_sleep`.
    pub(crate) wake_plan: WorkerWakePlan,
}

/// Wake targets chosen by worker-pool concurrency accounting.
///
/// The plan is computed under the pool lock during kick evaluation but must
/// only be executed after the pool lock is released. Outside the scheduler,
/// the provider wake interfaces (`WorkqueueHostIf::wake_system_worker` /
/// `wake_system_manager`) execute it; inside the scheduler, the run queue
/// enqueues the targets directly under its own lock.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WorkerWakePlan {
    /// Idle worker selected to take over queued work, if any.
    pub worker_to_wake: Option<WorkerId>,
    /// Whether the pool manager task should be woken to create a worker.
    pub should_wake_manager: bool,
}

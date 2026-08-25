// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::task::Waker;

use kcpu_id_map::LogicalCpuId;
use ktime_types::MonotonicInstant;

use super::{
    BottomHalfWorkQueueKind, SystemPoolBinding, SystemPoolKind, WorkerExecutionToken, WorkerId,
    WorkerWakePlan,
};

/// Scheduler-facing wake bridge for the system workerqueue.
///
/// `kwork` owns `system_wq` state and decides when a worker should be woken.
/// The task layer owns the actual kwork task and wait primitive.
#[kiface::interface]
pub trait WorkqueueHostIf {
    /// Returns the logical CPU currently executing the caller.
    ///
    /// Implementations must be callable from hardirq, serving-softirq, and
    /// BH-disabled context.
    fn current_cpu_id() -> LogicalCpuId;

    /// Returns whether the bounded worker pool for one CPU is installed and can
    /// drain queued work.
    ///
    /// Implementations must be callable from hardirq, serving-softirq, and
    /// BH-disabled context.
    fn is_system_pool_ready(pool_kind: SystemPoolKind, cpu_id: LogicalCpuId) -> bool;

    /// Wakes one idle worker waiting on one bound system workqueue CPU pool.
    ///
    /// Implementations must be callable from hardirq, serving-softirq, and
    /// BH-disabled context.
    fn wake_system_worker(pool_kind: SystemPoolKind, cpu_id: LogicalCpuId, worker_id: WorkerId);

    /// Wakes the task-context manager for one bound system worker pool.
    ///
    /// The manager may create worker tasks and therefore runs outside IRQ-safe
    /// enqueue paths.
    fn wake_system_manager(pool_kind: SystemPoolKind, cpu_id: LogicalCpuId);
}

/// IRQ-core bridge for bottom-half workerqueue execution.
///
/// Bottom-half queues are runtime instances executed by the IRQ subsystem, not
/// by provider-owned worker tasks. `kwork` only decides when one per-CPU BH
/// pool needs service; the provider maps that request to the appropriate
/// softirq vector.
#[kiface::interface]
pub trait WorkqueueBottomHalfIf {
    /// Raises the softirq that drains one bottom-half workerqueue lane.
    ///
    /// Implementations must be callable from hardirq, serving-softirq,
    /// BH-disabled, and task context.
    fn raise_bottom_half(kind: BottomHalfWorkQueueKind);
}

/// Scheduler-facing current-task context bridge for worker callbacks.
///
/// Worker-pool callbacks are sleepable and may yield.
/// `kwork` therefore records the currently executing work through
/// scheduler-owned task-local state rather than through per-CPU state that
/// could be invalidated by task migration. Bottom-half pools have a separate
/// non-sleepable execution contract.
#[kiface::interface]
pub trait WorkqueueTaskContextIf {
    /// Replaces the current task's workerqueue callback context and returns
    /// the previous context, if one existed.
    fn set_current_work_context(context: WorkqueueTaskContext) -> Option<WorkqueueTaskContext>;

    /// Clears the current task's workerqueue callback context if it still
    /// matches `context`.
    fn clear_current_work_context(context: WorkqueueTaskContext) -> bool;

    /// Returns the current task's workerqueue callback context, if one is
    /// executing.
    fn current_work_context() -> Option<WorkqueueTaskContext>;

    /// Refreshes the scheduler-owned tick deadline for the current worker.
    ///
    /// Called when a task enters or leaves a workerqueue callback so NOHZ does
    /// not stop the local timer past the CPU-intensive threshold.
    fn refresh_current_worker_tick();
}

/// Opaque workerqueue callback identity stored in scheduler task-local state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkqueueTaskContext {
    work_key: usize,
    queue_key: usize,
    pool_key: usize,
    worker_id: WorkerId,
    worker_token: WorkerExecutionToken,
}

impl WorkqueueTaskContext {
    /// Creates a task-local workerqueue callback context from opaque keys.
    pub const fn new(
        work_key: usize,
        queue_key: usize,
        pool_key: usize,
        worker_id: WorkerId,
        worker_token: WorkerExecutionToken,
    ) -> Self {
        Self {
            work_key,
            queue_key,
            pool_key,
            worker_id,
            worker_token,
        }
    }

    /// Returns the opaque work identity.
    pub const fn work_key(self) -> usize {
        self.work_key
    }

    /// Returns the opaque queue identity.
    pub const fn queue_key(self) -> usize {
        self.queue_key
    }

    /// Returns the opaque execution-pool identity.
    pub const fn pool_key(self) -> usize {
        self.pool_key
    }

    /// Returns the opaque worker identity inside its execution pool.
    pub const fn worker_id(self) -> WorkerId {
        self.worker_id
    }

    /// Returns the pool-local execution token for this worker slot.
    pub const fn worker_token(self) -> WorkerExecutionToken {
        self.worker_token
    }

    /// Marks this worker as blocked for worker-pool concurrency accounting.
    ///
    /// This is the X-Kernel counterpart of Linux `wq_worker_sleeping()`.
    /// Every task-context logical queue runs on a provider-backed shared pool
    /// and therefore participates in this accounting.
    ///
    /// Returns `true` when this call changed the worker from running to
    /// sleeping. Callers must only pair [`Self::worker_did_resume`] with a
    /// `true` result; nested sleepable waits may observe an already-sleeping
    /// worker and must not mark it running early.
    pub fn worker_will_block(self) -> bool {
        SystemPoolBinding::for_pool_key(self.pool_key)
            .is_some_and(|pool| pool.mark_worker_sleeping(self.worker_id))
    }

    /// Marks this worker as blocked for worker-pool concurrency accounting
    /// from scheduler context, without issuing any provider wake.
    ///
    /// This is the scheduler-context variant of [`Self::worker_will_block`].
    /// It performs the Running → Sleeping transition and the kick evaluation
    /// under the pool lock, then returns whether the worker really went to
    /// sleep together with the wake plan. The calling scheduler owns plan
    /// execution: its wake targets are same-pool tasks on the same run queue,
    /// so it enqueues them directly under its own run-queue lock instead of
    /// going through [`WorkqueueHostIf`] wake paths.
    ///
    /// Returns `(false, WorkerWakePlan::default())` when `pool_key` does not
    /// resolve to a system pool, matching the no-op behavior of
    /// [`Self::worker_will_block`].
    pub fn worker_will_block_in_scheduler(self) -> (bool, WorkerWakePlan) {
        match SystemPoolBinding::for_pool_key(self.pool_key) {
            Some(pool) => {
                let transition = pool.account_worker_sleeping(self.worker_id);
                (transition.did_sleep, transition.wake_plan)
            }
            None => (false, WorkerWakePlan::default()),
        }
    }

    /// Resolves this context's opaque pool key to its system pool binding.
    ///
    /// Scheduler hooks use the binding's `(kind, cpu)` identity to map a wake
    /// plan's worker id / manager flag to the provider tasks waiting on the
    /// same run queue. Returns `None` when `pool_key` does not identify a
    /// task-context system pool (for example a bottom-half pool key or an
    /// invalid key).
    pub fn system_pool_binding(self) -> Option<SystemPoolBinding> {
        SystemPoolBinding::for_pool_key(self.pool_key)
    }

    /// Marks this previously blocked system worker as runnable again.
    ///
    /// This is the X-Kernel counterpart of Linux `wq_worker_running()`.
    /// Scheduler resume hooks call this same method: resume accounting never
    /// issues wakes, so no scheduler-specific variant is needed.
    pub fn worker_did_resume(self) {
        if let Some(pool) = SystemPoolBinding::for_pool_key(self.pool_key) {
            pool.mark_worker_running(self.worker_id);
        }
    }

    /// Accounts a scheduler tick for this worker callback.
    ///
    /// This is the X-Kernel counterpart of Linux `wq_worker_tick()`: once the
    /// current callback exceeds the pool threshold, the live worker execution
    /// is marked CPU-intensive and leaves `nr_running`. The execution token
    /// prevents a delayed tick from changing a later work item that reused the
    /// same worker slot.
    pub fn worker_tick(self) {
        if let Some(pool) = SystemPoolBinding::for_pool_key(self.pool_key) {
            pool.mark_worker_tick(self.worker_id, self.worker_token);
        }
    }

    /// Accounts scheduler-context runtime for this worker callback.
    ///
    /// This is the scheduler-context variant of [`Self::worker_tick`]. It
    /// performs CPU-intensive marking under the pool lock and returns the wake
    /// plan to the caller, which must execute it without re-entering the
    /// external [`WorkqueueHostIf`] wake path.
    pub fn worker_tick_in_scheduler(self) -> WorkerWakePlan {
        match SystemPoolBinding::for_pool_key(self.pool_key) {
            Some(pool) => pool.account_worker_tick(self.worker_id, self.worker_token),
            None => WorkerWakePlan::default(),
        }
    }

    /// Returns the monotonic deadline at which this worker should receive a
    /// scheduler tick for CPU-intensive accounting.
    pub fn worker_tick_deadline(self) -> Option<MonotonicInstant> {
        SystemPoolBinding::for_pool_key(self.pool_key)
            .and_then(|pool| pool.worker_tick_deadline(self.worker_id, self.worker_token))
    }
}

/// Scheduler-facing wait bridge for workerqueue synchronization.
///
/// `kwork` owns the work lifecycle predicate, but it does not own task
/// blocking. The scheduler layer provides this interface so sleepable
/// workerqueue APIs can block without adding a `kwork -> ktask` dependency.
#[kiface::interface]
pub trait WorkqueueSyncWaitIf {
    /// Waits until the workerqueue completion wake source is observed.
    ///
    /// The completion is only a wake source. Implementations should follow the
    /// `try_wait/register/try_wait` protocol, and callers must recheck their
    /// real predicate after this method returns.
    fn wait_for_completion(completion: &kpoll::Completion) -> Result<(), kpoll::PollRegisterError>;

    /// Waits until the workerqueue completion or state-change wake source is observed.
    ///
    /// The completion/event sources are only wake sources. Implementations should
    /// follow the `check/register/recheck` protocol, and callers must recheck
    /// their real predicate after this method returns.
    fn wait_for_completion_or_event(
        completion: &kpoll::Completion,
        state_change: &kpoll::PollEvent,
        observed_generation: usize,
    ) -> Result<(), kpoll::PollRegisterError>;
}

/// Scheduler-facing timer bridge for delayed work.
///
/// `kwork` owns delayed-work state, while the scheduler/task layer owns the
/// timer wheel and hardware rearm policy.
pub trait WorkqueueTimerHandle: Send + Sync {
    /// Cancels the registered timer if it is still pending.
    ///
    /// The operation must be safe from any CPU. It may race with a timer that
    /// is already firing; in that case the delayed-work instance/generation
    /// check in `kwork` decides whether the firing callback is stale.
    fn cancel(&self);
}

#[kiface::interface]
pub trait WorkqueueTimerIf {
    /// Returns the current monotonic time.
    ///
    /// Implementations must be callable from hardirq, serving-softirq, and
    /// BH-disabled context: worker pools read this under the pool lock while
    /// evaluating CPU-intensive marking from IRQ-safe enqueue paths. It must
    /// not allocate or take any sleeping lock.
    fn monotonic_time() -> MonotonicInstant;

    /// Registers `waker` to fire at `deadline`.
    ///
    /// Returns `None` when the deadline is already expired and no timer was
    /// installed. Callers may then queue the work immediately.
    fn register_timer(
        deadline: MonotonicInstant,
        waker: Waker,
    ) -> Option<Arc<dyn WorkqueueTimerHandle>>;
}

/// Execution-context bridge for waiting workerqueue APIs.
///
/// `kwork` must reject waits from hardirq, serving-softirq, and BH-disabled
/// contexts, but those context bits are owned by the interrupt core. The IRQ
/// layer provides this predicate without making `kwork` depend on `kirq`.
#[kiface::interface]
pub trait WorkqueueContextIf {
    /// Returns whether the current execution context cannot sleep for a
    /// workerqueue synchronization wait.
    fn is_invalid_wait_context() -> bool;
}

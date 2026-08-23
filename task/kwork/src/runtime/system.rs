// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kcpu_id_map::LogicalCpuId;

use super::{static_array_index, static_array_index_by_key};
use crate::{
    QueueWorkResult, ScheduledWork, WorkQueue, WorkerExecutionToken, WorkerId, WorkerPool,
    WorkerPoolAttrs, WorkerPoolCpuAffinity, WorkerPoolExecution, WorkerPoolSchedulingPolicy,
    WorkerSleepTransition, WorkerWakePlan, WorkqueueHostIf,
};

/// Number of system workers started during CPU workerqueue bring-up.
///
/// Additional workers are created by the task-context system worker manager
/// when all running workers block while runnable work remains.
pub const INITIAL_SYSTEM_WORKERS_PER_CPU: usize = 1;

/// Maximum number of workers in one per-CPU system pool.
///
/// The system pool avoids worker creation from IRQ-safe enqueue paths. Dynamic
/// creation is delegated to the provider-owned manager task and stays bounded
/// by the build-time `WORKQUEUE_WORKERS_PER_POOL` configuration symbol until
/// idle culling is added.
pub const MAX_SYSTEM_WORKERS_PER_CPU: usize = kbuild_config::WORKQUEUE_WORKERS_PER_POOL;

/// Built-in system workerqueue kind.
///
/// This enum covers task-context system queues. Bottom-half queues are separate
/// runtime instances in `runtime::bh`; Linux-like creation flags such as
/// high-priority, unbound, freezable, and reclaim remain unsupported until
/// their backing policy exists.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SystemWorkQueueKind {
    /// Default per-CPU system queue for short common work.
    Default,
    /// Per-CPU queue for long-running work.
    ///
    /// This is a separate queue and flush/accounting domain from
    /// [`Self::Default`], but shares the same default per-CPU worker pool.
    Long,
}

impl SystemWorkQueueKind {
    /// All built-in system queue kinds.
    pub const ALL: [Self; Self::COUNT] = [Self::Default, Self::Long];
    /// Number of built-in system queue kinds.
    pub const COUNT: usize = 2;

    /// Returns the stable index used by provider-side per-kind arrays.
    pub const fn as_usize(self) -> usize {
        match self {
            Self::Default => 0,
            Self::Long => 1,
        }
    }

    /// Returns the built-in queue name.
    pub const fn queue_name(self) -> &'static str {
        match self {
            Self::Default => "system_wq",
            Self::Long => "system_long_wq",
        }
    }
}

static SYSTEM_WORKQUEUES: [[WorkQueue; kbuild_config::NR_CPUS]; SystemWorkQueueKind::COUNT] = [
    [const { WorkQueue::new("system_wq") }; kbuild_config::NR_CPUS],
    [const { WorkQueue::new("system_long_wq") }; kbuild_config::NR_CPUS],
];

static SYSTEM_POOLS: [WorkerPool; kbuild_config::NR_CPUS] =
    [const { WorkerPool::new() }; kbuild_config::NR_CPUS];

/// Handle to one per-CPU system pool binding.
///
/// This is a binding view over the per-CPU system [`WorkQueue`], the shared
/// [`WorkerPool`] execution state, and the queue-owned `WorkQueuePoolState`
/// accounting state connecting the two. Scheduler providers use this binding
/// during CPU bring-up and from worker-task wait loops; ordinary producers
/// should keep using [`crate::schedule_work`] and the [`WorkQueue`]
/// enqueue APIs.
#[derive(Clone, Copy)]
pub struct SystemPoolBinding {
    kind: SystemWorkQueueKind,
    cpu_id: LogicalCpuId,
    queue: &'static WorkQueue,
    pool: &'static WorkerPool,
}

/// Generic task-context execution binding used by core queue/binding code.
///
/// The contained system binding is a runtime/provider detail: it maps the
/// generic bound per-CPU pool selected by the core model to the currently
/// available task-context provider lane. Keeping this wrapper private to the
/// runtime boundary prevents `SystemWorkQueueKind` from becoming a field in
/// the core `pool_workqueue` model.
#[derive(Clone, Copy)]
pub(crate) struct TaskPoolBinding {
    system: SystemPoolBinding,
}

/// Provider wake request for one task-context execution binding.
#[derive(Clone, Copy)]
pub(crate) struct TaskPoolWake {
    binding: TaskPoolBinding,
    plan: WorkerWakePlan,
}

impl SystemPoolBinding {
    fn new(
        kind: SystemWorkQueueKind,
        cpu_id: LogicalCpuId,
        queue: &'static WorkQueue,
        pool: &'static WorkerPool,
    ) -> Self {
        pool.ensure_attrs(WorkerPoolAttrs::new(
            WorkerPoolExecution::Task,
            WorkerPoolSchedulingPolicy::Normal,
            WorkerPoolCpuAffinity::Pinned(cpu_id),
        ));
        Self {
            kind,
            cpu_id,
            queue,
            pool,
        }
    }

    /// Resolves the system worker pool binding for one queue kind and CPU.
    ///
    /// Returns `None` if `cpu_id` is outside `NR_CPUS`.
    pub fn for_kind_cpu(kind: SystemWorkQueueKind, cpu_id: LogicalCpuId) -> Option<Self> {
        Some(Self::new(
            SystemWorkQueueKind::Default,
            cpu_id,
            system_wq_for_kind_cpu(kind, cpu_id)?,
            system_pool_for_cpu(cpu_id)?,
        ))
    }

    /// Resolves the system worker pool that owns `pool_key`.
    ///
    /// The pool key identifies the shared execution pool, independent of which
    /// logical workqueue owns the current work.
    pub(crate) fn for_pool_key(pool_key: usize) -> Option<Self> {
        let (kind, cpu_id) = system_pool_kind_cpu_by_key(pool_key)?;
        Self::for_kind_cpu(kind, cpu_id)
    }

    /// Returns the system worker-pool kind.
    ///
    /// `system_long_wq` has its own queue object but shares the default
    /// per-CPU task worker pool, so long-queue bindings also return
    /// [`SystemWorkQueueKind::Default`] here.
    pub fn kind(self) -> SystemWorkQueueKind {
        self.kind
    }

    /// Returns the logical CPU this pool is bound to.
    pub fn cpu_id(self) -> LogicalCpuId {
        self.cpu_id
    }

    /// Returns the system workqueue bound to this pool.
    pub fn queue(self) -> &'static WorkQueue {
        self.queue
    }

    pub(crate) fn pool(self) -> &'static WorkerPool {
        self.pool
    }

    /// Returns the execution attributes of this system worker pool.
    pub fn attrs(self) -> WorkerPoolAttrs {
        self.pool
            .attrs()
            .expect("system pool binding should initialize worker-pool attrs")
    }

    /// Marks one pre-created system worker as installed for this pool.
    ///
    /// The task provider calls this during CPU bring-up before activating the
    /// corresponding `kwork/system_wq/<cpu>:<id>` task. A system pool is
    /// ready for enqueue after its initial worker has been installed.
    pub fn install_worker(self, worker_id: WorkerId) -> bool {
        if worker_id.as_usize() >= MAX_SYSTEM_WORKERS_PER_CPU {
            warn!(
                "cannot install out-of-range {:?} system worker {} for CPU {}",
                self.kind(),
                worker_id.as_usize(),
                self.cpu_id().as_usize()
            );
            return false;
        }

        self.pool()
            .state
            .lock()
            .install_worker(worker_id.as_usize())
    }

    /// Reserves an empty system worker slot for task-context worker creation.
    ///
    /// The returned slot is marked `Creating`, so concurrent manager wakeups
    /// cannot choose the same worker id. The caller must later call
    /// [`Self::finish_worker_creation`] with the creation result.
    pub fn reserve_worker_creation(self) -> Option<WorkerId> {
        self.pool()
            .state
            .lock()
            .reserve_worker_creation()
            .map(WorkerId::new)
    }

    /// Completes a system worker creation attempt and wakes the next required
    /// actor.
    pub fn finish_worker_creation(self, worker_id: WorkerId, success: bool) {
        let wake_plan = self
            .pool()
            .state
            .lock()
            .finish_worker_creation(worker_id.as_usize(), success);
        wake_system_pool_plan(self.kind(), self.cpu_id(), wake_plan);
    }

    /// Returns whether this pool has a pending manager request.
    pub fn manager_needed(self) -> bool {
        let pool_state = self.pool().state.lock();
        // A frozen pool must not create workers: the test owns the pool and
        // drains it manually.
        #[cfg(unittest)]
        if pool_state.is_wake_frozen_for_tests() {
            return false;
        }
        pool_state.manager_should_run()
    }

    /// Test-only: freezes wake execution so the test owns this pool.
    ///
    /// While frozen, wake plans are buffered (see `wake_system_pool_plan`),
    /// awake workers re-block instead of taking work, and the manager reports
    /// no pending request. Provider drain loops must additionally stop taking
    /// work via [`Self::wakes_frozen_for_tests`] checks.
    #[cfg(unittest)]
    pub fn freeze_wakes_for_tests(self) {
        self.pool().state.lock().freeze_wakes_for_tests();
    }

    /// Test-only: returns whether wake execution is frozen for this pool.
    #[cfg(unittest)]
    pub fn wakes_frozen_for_tests(self) -> bool {
        self.pool().state.lock().is_wake_frozen_for_tests()
    }

    /// Test-only: ends a wake freeze and executes or discards buffered wakes.
    ///
    /// With `discard_deferred` the buffered plan is dropped and the pool is
    /// left quiescent; otherwise the merged buffered plan is executed so live
    /// workers drain the work queued while frozen.
    #[cfg(unittest)]
    pub fn unfreeze_wakes_for_tests(self, discard_deferred: bool) {
        let wake_plan = self
            .pool()
            .state
            .lock()
            .unfreeze_wakes_for_tests(discard_deferred);
        wake_system_pool_plan(self.kind(), self.cpu_id(), wake_plan);
    }

    /// Prepares one system worker to wait for work and returns whether work is
    /// runnable now.
    ///
    /// A worker woken by wake-one accounting enters `Preparing` before it
    /// reaches the ktask wait loop. If the queued work was cancelled before
    /// the worker could drain it, this function clears `Preparing` back to
    /// `Idle` before the worker registers its wait source. That keeps later
    /// enqueue operations from suppressing a needed wake because of a stale
    /// preparing slot.
    pub fn prepare_worker_to_wait(self, worker_id: WorkerId) -> bool {
        if worker_id.as_usize() >= MAX_SYSTEM_WORKERS_PER_CPU {
            return false;
        }
        self.pool()
            .state
            .lock()
            .prepare_worker_to_wait(worker_id.as_usize())
    }

    /// Returns whether this pool has runnable work.
    ///
    /// Only entries that have reached runnable state wake the system worker
    /// pool.
    pub fn has_runnable_work(self) -> bool {
        self.pool().state.lock().has_runnable_work()
    }

    /// Runs one runnable work item from this shared worker pool.
    ///
    /// The selected work may belong to any logical workqueue attached to this
    /// pool. This mirrors Linux workers draining `pool->worklist`, where the
    /// work's binding identifies the logical queue used for accounting.
    pub fn run_one_work(self, worker_id: WorkerId) -> bool {
        crate::process_one_pool_work(self, worker_id)
    }

    /// Accounts this pool's worker blocking without waking anyone.
    ///
    /// Performs the Running → Sleeping transition and kick evaluation under the
    /// pool lock and returns the accounting result with the wake plan. The
    /// caller owns plan execution: scheduler-context callers enqueue the plan
    /// targets under their own run-queue lock, while the wrapper path uses
    /// [`Self::mark_worker_sleeping`].
    pub(crate) fn account_worker_sleeping(self, worker_id: WorkerId) -> WorkerSleepTransition {
        let worker_index = worker_id.as_usize();
        if worker_index >= MAX_SYSTEM_WORKERS_PER_CPU {
            return WorkerSleepTransition {
                did_sleep: false,
                wake_plan: WorkerWakePlan::default(),
            };
        }
        self.pool().state.lock().mark_worker_sleeping(worker_index)
    }

    /// Marks this pool's worker as blocked for worker-pool concurrency
    /// accounting and executes the resulting wake plan outside the pool lock.
    ///
    /// This is the X-Kernel counterpart of Linux `wq_worker_sleeping()`.
    /// Returns `true` when this call changed the worker from running to
    /// sleeping. Callers must only pair
    /// [`crate::WorkqueueTaskContext::worker_did_resume`] with a `true` result;
    /// nested sleepable waits may observe an already-sleeping worker and must
    /// not mark it running early.
    pub(crate) fn mark_worker_sleeping(self, worker_id: WorkerId) -> bool {
        let transition = self.account_worker_sleeping(worker_id);
        wake_system_pool_plan(self.kind(), self.cpu_id(), transition.wake_plan);
        transition.did_sleep
    }

    /// Marks a previously blocked system worker as runnable again.
    ///
    /// This is the X-Kernel counterpart of Linux `wq_worker_running()`.
    pub(crate) fn mark_worker_running(self, worker_id: WorkerId) {
        let worker_index = worker_id.as_usize();
        if worker_index >= MAX_SYSTEM_WORKERS_PER_CPU {
            return;
        }
        self.pool().state.lock().mark_worker_running(worker_index);
    }

    /// Accounts a scheduler tick for one running worker without waking anyone.
    ///
    /// The caller owns plan execution. Scheduler-context callers enqueue the
    /// plan targets under their own run-queue lock, while
    /// [`Self::mark_worker_tick`] executes the same plan through
    /// [`WorkqueueHostIf`].
    pub(crate) fn account_worker_tick(
        self,
        worker_id: WorkerId,
        worker_token: WorkerExecutionToken,
    ) -> WorkerWakePlan {
        let worker_index = worker_id.as_usize();
        if worker_index >= MAX_SYSTEM_WORKERS_PER_CPU {
            return WorkerWakePlan::default();
        }
        self.pool()
            .state
            .lock()
            .tick_running_worker(worker_index, worker_token)
    }

    /// Accounts a scheduler tick for one running worker and executes any wake
    /// plan selected after CPU-intensive marking.
    pub(crate) fn mark_worker_tick(self, worker_id: WorkerId, worker_token: WorkerExecutionToken) {
        let wake_plan = self.account_worker_tick(worker_id, worker_token);
        wake_system_pool_plan(self.kind(), self.cpu_id(), wake_plan);
    }

    pub(crate) fn is_installed(self) -> bool {
        self.pool().state.lock().installed_worker_count() >= INITIAL_SYSTEM_WORKERS_PER_CPU
    }
}

impl TaskPoolBinding {
    /// Resolves the default task-context pool bound to `cpu_id`.
    pub(crate) fn default_for_cpu(cpu_id: LogicalCpuId) -> Option<Self> {
        Self::from_ready_system(SystemPoolBinding::for_kind_cpu(
            SystemWorkQueueKind::Default,
            cpu_id,
        )?)
    }

    /// Resolves the task-context pool used by a static queue on `cpu_id`.
    pub(crate) fn for_static_queue_cpu(
        queue: &'static WorkQueue,
        cpu_id: LogicalCpuId,
    ) -> Option<Self> {
        let system = match system_wq_kind_cpu(queue) {
            Some((kind, _queue_cpu)) => {
                let system = SystemPoolBinding::for_kind_cpu(kind, cpu_id)?;
                if !core::ptr::eq(system.queue(), queue) {
                    return None;
                }
                system
            }
            None => SystemPoolBinding::for_kind_cpu(SystemWorkQueueKind::Default, cpu_id)?,
        };
        Self::from_ready_system(system)
    }

    /// Wraps a provider-visible system pool binding for generic core code.
    ///
    /// Provider entry points resolve concrete system instances, but binding/pool
    /// accounting only needs the task-context execution pool and CPU-local binding
    /// index. This conversion is intentionally unchecked: the caller already
    /// owns a live provider binding.
    pub(crate) fn from_system_binding(system: SystemPoolBinding) -> Self {
        Self { system }
    }

    fn from_ready_system(system: SystemPoolBinding) -> Option<Self> {
        if !WorkqueueHostIf::is_system_pool_ready(system.kind(), system.cpu_id())
            || !system.is_installed()
        {
            return None;
        }
        Some(Self { system })
    }

    pub(crate) fn cpu_id(self) -> LogicalCpuId {
        self.system.cpu_id()
    }

    pub(crate) fn pool(self) -> &'static WorkerPool {
        self.system.pool()
    }

    pub(crate) fn wake(self, plan: WorkerWakePlan) -> TaskPoolWake {
        TaskPoolWake {
            binding: self,
            plan,
        }
    }
}

impl TaskPoolWake {
    pub(crate) fn execute(self) {
        let system = self.binding.system;
        wake_system_pool_plan(system.kind(), system.cpu_id(), self.plan);
    }
}

/// Returns the current CPU's default bound per-CPU system workerqueue.
///
/// This accessor depends on [`WorkqueueHostIf`] being available so `kwork`
/// can resolve the caller's current logical CPU.
///
/// This is the X-Kernel runtime counterpart of Linux `system_percpu_wq`, the
/// queue used by `schedule_work*()` and `schedule_delayed_work*()` helpers.
/// The name describes the public runtime instance; the underlying pool/binding
/// model remains the generic bound per-CPU workqueue model.
///
/// # Panics
///
/// Panics if the provider returns a logical CPU id outside `NR_CPUS`.
pub fn system_percpu_wq() -> &'static WorkQueue {
    system_percpu_wq_for_cpu(WorkqueueHostIf::current_cpu_id())
        .expect("current logical CPU must have a per-CPU system workerqueue lane")
}

/// Returns the current CPU's default bound system workerqueue.
///
/// Compatibility alias for [`system_percpu_wq`]. New code should use
/// [`system_percpu_wq`] to match Linux, where `system_wq` is a deprecated
/// compatibility instance and `schedule_work*()` targets `system_percpu_wq`.
///
/// # Panics
///
/// Panics if the provider returns a logical CPU id outside `NR_CPUS`.
pub fn system_wq() -> &'static WorkQueue {
    system_percpu_wq()
}

/// Returns the current CPU's long-running bound system workerqueue.
///
/// Long system work has a separate queue and flush/accounting domain from
/// [`system_wq`], but shares the same default per-CPU worker pool.
///
/// # Panics
///
/// Panics if the provider returns a logical CPU id outside `NR_CPUS`.
pub fn system_long_wq() -> &'static WorkQueue {
    system_long_wq_for_cpu(WorkqueueHostIf::current_cpu_id())
        .expect("current logical CPU must have a long system workerqueue lane")
}

/// Returns the default bound per-CPU system workerqueue for `cpu_id`.
///
/// This is the X-Kernel runtime counterpart of Linux `system_percpu_wq` for a
/// specific CPU.
///
/// Returns `None` if the logical CPU id is outside `NR_CPUS`.
pub fn system_percpu_wq_for_cpu(cpu_id: LogicalCpuId) -> Option<&'static WorkQueue> {
    system_wq_for_kind_cpu(SystemWorkQueueKind::Default, cpu_id)
}

/// Returns the default bound system workerqueue for `cpu_id`.
///
/// Compatibility alias for [`system_percpu_wq_for_cpu`].
///
/// Returns `None` if the logical CPU id is outside `NR_CPUS`.
pub fn system_wq_for_cpu(cpu_id: LogicalCpuId) -> Option<&'static WorkQueue> {
    system_percpu_wq_for_cpu(cpu_id)
}

/// Returns the long-running bound system workerqueue for `cpu_id`.
///
/// Returns `None` if the logical CPU id is outside `NR_CPUS`.
pub fn system_long_wq_for_cpu(cpu_id: LogicalCpuId) -> Option<&'static WorkQueue> {
    system_wq_for_kind_cpu(SystemWorkQueueKind::Long, cpu_id)
}

/// Queues `work` on the current CPU's default system workerqueue.
pub fn schedule_work(work: &ScheduledWork) -> QueueWorkResult {
    system_percpu_wq().queue_work(work)
}

/// Queues `work` on the default system workerqueue bound to `cpu_id`.
pub fn schedule_work_on(cpu_id: LogicalCpuId, work: &ScheduledWork) -> QueueWorkResult {
    let Some(queue) = system_percpu_wq_for_cpu(cpu_id) else {
        return QueueWorkResult::InvalidCpu;
    };
    queue.queue_work(work)
}

/// Queues `work` on the current CPU's long-running system workerqueue.
pub fn schedule_long_work(work: &ScheduledWork) -> QueueWorkResult {
    system_long_wq().queue_work(work)
}

/// Queues `work` on the long-running system workerqueue bound to `cpu_id`.
pub fn schedule_long_work_on(cpu_id: LogicalCpuId, work: &ScheduledWork) -> QueueWorkResult {
    let Some(queue) = system_long_wq_for_cpu(cpu_id) else {
        return QueueWorkResult::InvalidCpu;
    };
    queue.queue_work(work)
}

pub(crate) fn system_wq_for_kind_cpu(
    kind: SystemWorkQueueKind,
    cpu_id: LogicalCpuId,
) -> Option<&'static WorkQueue> {
    SYSTEM_WORKQUEUES
        .get(kind.as_usize())
        .and_then(|queues| queues.get(cpu_id.as_usize()))
}

pub(crate) fn system_wq_kind_cpu(
    queue: &'static WorkQueue,
) -> Option<(SystemWorkQueueKind, LogicalCpuId)> {
    for kind in SystemWorkQueueKind::ALL {
        if let Some(cpu_id) =
            static_array_index(&SYSTEM_WORKQUEUES[kind.as_usize()], queue).map(LogicalCpuId::new)
        {
            return Some((kind, cpu_id));
        }
    }
    None
}

pub(crate) fn system_wq_cpu(queue: &'static WorkQueue) -> Option<LogicalCpuId> {
    system_wq_kind_cpu(queue).map(|(_, cpu_id)| cpu_id)
}

pub(crate) fn system_pool_for_cpu(cpu_id: LogicalCpuId) -> Option<&'static WorkerPool> {
    SYSTEM_POOLS.get(cpu_id.as_usize())
}

pub(crate) fn system_pool_kind_cpu_by_key(
    pool_key: usize,
) -> Option<(SystemWorkQueueKind, LogicalCpuId)> {
    static_array_index_by_key(&SYSTEM_POOLS, pool_key)
        .map(|cpu_id| (SystemWorkQueueKind::Default, LogicalCpuId::new(cpu_id)))
}

pub(crate) fn wake_system_pool_plan(
    kind: SystemWorkQueueKind,
    cpu_id: LogicalCpuId,
    plan: WorkerWakePlan,
) {
    // Test-only: a frozen pool buffers wake plans instead of executing them,
    // so live workers cannot drain work a test wants to control.
    #[cfg(unittest)]
    if SystemPoolBinding::for_kind_cpu(kind, cpu_id)
        .is_some_and(|binding| binding.pool().state.lock().defer_wake_plan_for_tests(plan))
    {
        return;
    }
    if let Some(worker_id) = plan.worker_to_wake {
        WorkqueueHostIf::wake_system_worker(kind, cpu_id, worker_id);
    }
    if plan.should_wake_manager {
        WorkqueueHostIf::wake_system_manager(kind, cpu_id);
    }
}

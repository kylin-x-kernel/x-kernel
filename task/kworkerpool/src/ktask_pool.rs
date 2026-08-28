// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ktask-backed runtime wrapper for worker-pool state.

use alloc::{string::String, sync::Arc};
use core::{future::poll_fn, task::Poll};

use kcpu_id_map::LogicalCpuId;
use kpoll::{PollEvent, PollRegistrations};
use kspin::{SpinNoIrq, SpinNoIrqGuard};
use ktime_types::MonotonicInstant;

use crate::{
    ActionBatch, ImmediateAction, ManagementAction, ManagementComplete, ManagerRuntime,
    ManagerTarget, ManagerTask, PoolId, WorkerId, WorkerPool, WorkerPoolPolicy, WorkerRuntime,
    WorkerTask, WorkerThreadRef, decode_task_context, prepare_bound_kthread, worker_name,
};

/// Builds the runtime object used by one ktask-backed worker.
///
/// The factory is supplied by the product layer. It receives only worker-pool
/// runtime identity and this worker's wait source; callback storage and
/// workqueue semantics remain outside `kworkerpool`.
pub type WorkerRuntimeFactory<R> = fn(PoolId, WorkerId, Arc<PollEvent>) -> R;

/// Converts a pool identity into a runtime-visible name component.
pub type PoolNameResolver = fn(PoolId) -> &'static str;

/// Result of accounting one ktask execution tick for a worker-pool context.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionTickResult {
    /// Runtime actions produced by CPU-intensive accounting.
    pub actions: ActionBatch,
    /// Next CPU-intensive accounting deadline, if the current execution still
    /// needs one.
    pub deadline: Option<MonotonicInstant>,
}

/// ktask-backed runtime wrapper around one core worker-pool instance.
///
/// The core pool owns scheduling state. This wrapper owns ktask-specific
/// objects attached to that state: worker tasks, worker wake sources, and the
/// manager task wake source.
pub struct KtaskWorkerPool<const MAX_WORKERS: usize, const ENTRY_CAP: usize> {
    inner: SpinNoIrq<WorkerPool<WorkerThreadRef, MAX_WORKERS, ENTRY_CAP>>,
    manager_thread: SpinNoIrq<Option<WorkerThreadRef>>,
}

impl<const MAX_WORKERS: usize, const ENTRY_CAP: usize> KtaskWorkerPool<MAX_WORKERS, ENTRY_CAP> {
    /// Creates one ktask-backed pool instance.
    pub const fn new(id: PoolId, policy: WorkerPoolPolicy) -> Self {
        Self {
            inner: SpinNoIrq::new(WorkerPool::new(id, policy)),
            manager_thread: SpinNoIrq::new(None),
        }
    }

    /// Locks the core worker-pool state.
    pub fn lock(&self) -> SpinNoIrqGuard<'_, WorkerPool<WorkerThreadRef, MAX_WORKERS, ENTRY_CAP>> {
        self.inner.lock()
    }

    /// Tries to lock the core worker-pool state without spinning.
    pub fn try_lock(
        &self,
    ) -> Option<SpinNoIrqGuard<'_, WorkerPool<WorkerThreadRef, MAX_WORKERS, ENTRY_CAP>>> {
        self.inner.try_lock()
    }

    /// Returns this pool identity.
    pub fn id(&self) -> PoolId {
        self.inner.lock().id()
    }

    /// Reconfigures an empty const-initialized pool.
    pub fn configure_empty(&self, id: PoolId, policy: WorkerPoolPolicy) -> bool {
        self.inner.lock().configure_empty(id, policy)
    }

    /// Installs one already-created worker task into a pool slot.
    pub fn install_worker(&self, worker: WorkerId, thread_ref: WorkerThreadRef) -> bool {
        self.inner.lock().install_worker(worker, thread_ref).is_ok()
    }

    /// Installs the manager task used to drive slow-path lifecycle actions.
    pub fn install_manager_thread(&self, thread_ref: WorkerThreadRef) -> bool {
        let mut slot = self.manager_thread.lock();
        if slot.is_some() {
            return false;
        }
        *slot = Some(thread_ref);
        true
    }

    /// Returns one worker thread reference.
    pub fn worker_thread_ref(&self, worker: WorkerId) -> Option<WorkerThreadRef> {
        self.inner.lock().worker_thread_ref(worker).cloned()
    }

    /// Returns the manager thread reference.
    pub fn manager_thread_ref(&self) -> Option<WorkerThreadRef> {
        self.manager_thread.lock().clone()
    }

    /// Wakes one worker, if the slot is installed.
    pub fn wake_worker(&self, worker: WorkerId) -> bool {
        let Some(thread_ref) = self.worker_thread_ref(worker) else {
            return false;
        };
        thread_ref.wake();
        true
    }

    /// Wakes this pool's manager task, if one is installed.
    pub fn wake_manager(&self) -> bool {
        let Some(thread_ref) = self.manager_thread_ref() else {
            return false;
        };
        thread_ref.wake();
        true
    }

    /// Returns one ktask-backed worker task.
    pub fn worker_task(&self, worker: WorkerId) -> Option<ktask::KtaskRef> {
        self.worker_thread_ref(worker)?.task()
    }

    /// Returns this pool's manager task.
    pub fn manager_task(&self) -> Option<ktask::KtaskRef> {
        self.manager_thread_ref()?.task()
    }

    /// Accounts a ktask execution tick if `context` belongs to this pool.
    pub fn account_execution_tick(
        &self,
        context: ktask::TaskExecutionContext,
        now: MonotonicInstant,
    ) -> Option<ExecutionTickResult> {
        let parts = decode_task_context(context)?;
        if parts.pool_id != self.id() {
            return None;
        }

        let actions = self
            .inner
            .lock()
            .worker_tick(parts.worker, parts.token, now)
            .ok()?;
        let deadline = self
            .inner
            .lock()
            .worker_tick_deadline(parts.worker, parts.token);
        Some(ExecutionTickResult { actions, deadline })
    }

    /// Returns the execution tick deadline if `context` belongs to this pool.
    pub fn execution_tick_deadline(
        &self,
        context: ktask::TaskExecutionContext,
    ) -> Option<MonotonicInstant> {
        let parts = decode_task_context(context)?;
        if parts.pool_id != self.id() {
            return None;
        }
        self.inner
            .lock()
            .worker_tick_deadline(parts.worker, parts.token)
    }

    /// Accounts that a ktask-backed worker blocked in a sleepable wait.
    pub fn account_worker_blocked(
        &self,
        context: ktask::TaskExecutionContext,
        now: MonotonicInstant,
    ) -> Option<ActionBatch> {
        let parts = decode_task_context(context)?;
        let mut inner = self.inner.lock();
        if parts.pool_id != inner.id() {
            return None;
        }
        inner.worker_blocked(parts.worker, parts.token, now).ok()
    }

    /// Accounts that a ktask-backed worker resumed after a sleepable wait.
    pub fn account_worker_resumed(&self, context: ktask::TaskExecutionContext) -> bool {
        let Some(parts) = decode_task_context(context) else {
            return false;
        };
        let mut inner = self.inner.lock();
        if parts.pool_id != inner.id() {
            return false;
        }
        inner.worker_resumed(parts.worker, parts.token).is_ok()
    }

    /// Creates and activates one worker task for `worker`.
    pub fn start_worker_task<R>(
        &'static self,
        worker: WorkerId,
        pool_name: &'static str,
        runtime_factory: WorkerRuntimeFactory<R>,
    ) -> bool
    where
        R: WorkerRuntime<&'static Self> + 'static,
    {
        let pool_id = self.id();
        let wake_source = Arc::new(PollEvent::new());
        let thread_wake_source = wake_source.clone();
        let task = prepare_bound_kthread(pool_id.cpu(), worker_name(pool_id, worker, pool_name), {
            move || {
                // The pool may clear its WorkerThreadRef when this worker begins
                // retirement. Hold a task ref cloned from ktask's current-task
                // slot until the entry closure returns into ktask's exit path.
                let _entry_task_ref = ktask::current().clone();
                let runtime = runtime_factory(pool_id, worker, wake_source);
                WorkerTask::new(self, worker, runtime).run();
            }
        });

        if !self.install_worker(
            worker,
            WorkerThreadRef::new(task.clone(), thread_wake_source),
        ) {
            warn!(
                "failed to install workerpool worker {} for pool kind {} CPU {}",
                worker.as_usize(),
                pool_id.kind().as_usize(),
                pool_id.cpu().as_usize()
            );
            return false;
        }

        ktask::activate_task(&task);
        true
    }
}

impl<const MAX_WORKERS: usize, const ENTRY_CAP: usize> ManagerTarget
    for &'static KtaskWorkerPool<MAX_WORKERS, ENTRY_CAP>
{
    fn manager_should_run(&self, now: MonotonicInstant) -> bool {
        self.lock().manager_should_run(now)
    }

    fn next_management_deadline(&self, now: MonotonicInstant) -> Option<MonotonicInstant> {
        self.lock().next_management_deadline(now)
    }

    fn next_management_action(&self, now: MonotonicInstant) -> Option<ManagementAction> {
        self.lock().next_management_action(now)
    }

    fn complete_worker_spawn(
        &self,
        worker: WorkerId,
        success: bool,
        now: MonotonicInstant,
    ) -> ActionBatch {
        let result = if success {
            ManagementComplete::Spawned
        } else {
            ManagementComplete::SpawnFailed
        };
        self.lock()
            .spawn_complete(worker, result, now)
            .unwrap_or_default()
    }
}

struct KtaskManagerRuntime<R, const MAX_WORKERS: usize, const ENTRY_CAP: usize>
where
    R: WorkerRuntime<&'static KtaskWorkerPool<MAX_WORKERS, ENTRY_CAP>> + 'static,
{
    runtime_factory: WorkerRuntimeFactory<R>,
    pool_name: PoolNameResolver,
    wake_source: Arc<PollEvent>,
}

impl<R, const MAX_WORKERS: usize, const ENTRY_CAP: usize>
    ManagerRuntime<&'static KtaskWorkerPool<MAX_WORKERS, ENTRY_CAP>>
    for KtaskManagerRuntime<R, MAX_WORKERS, ENTRY_CAP>
where
    R: WorkerRuntime<&'static KtaskWorkerPool<MAX_WORKERS, ENTRY_CAP>> + 'static,
{
    fn now(&self) -> MonotonicInstant {
        ktask::monotonic_time()
    }

    fn wait_for_manager_work<const N: usize>(
        &mut self,
        pools: &[&'static KtaskWorkerPool<MAX_WORKERS, ENTRY_CAP>; N],
    ) {
        wait_for_manager_work(pools, &self.wake_source);
    }

    fn spawn_worker(
        &mut self,
        pool: &&'static KtaskWorkerPool<MAX_WORKERS, ENTRY_CAP>,
        worker: WorkerId,
    ) -> bool {
        pool.start_worker_task(worker, (self.pool_name)(pool.id()), self.runtime_factory)
    }

    fn wake_retiring_worker(
        &mut self,
        pool: &&'static KtaskWorkerPool<MAX_WORKERS, ENTRY_CAP>,
        worker: WorkerId,
    ) {
        let _ = pool.wake_worker(worker);
    }

    fn handle_pool_actions(
        &mut self,
        pool: &&'static KtaskWorkerPool<MAX_WORKERS, ENTRY_CAP>,
        actions: ActionBatch,
    ) {
        for action in actions.immediate() {
            match action {
                ImmediateAction::WakeWorker {
                    pool: action_pool,
                    worker,
                } => {
                    if action_pool == pool.id() {
                        let _ = pool.wake_worker(worker);
                    }
                }
                ImmediateAction::WakeManager { pool: action_pool } => {
                    if action_pool == pool.id() {
                        let _ = pool.wake_manager();
                    }
                }
                ImmediateAction::RaiseBottomHalf { .. }
                | ImmediateAction::ArmCpuIntensiveTimer { .. } => {}
            }
        }
    }
}

/// Creates and activates one per-CPU manager task for a homogeneous pool set.
///
/// The same manager wake source is installed into every managed pool in
/// `pools`, so any pool can request slow-path lifecycle work without owning its
/// own manager task.
pub fn start_manager_task<R, const MAX_WORKERS: usize, const ENTRY_CAP: usize, const N: usize>(
    cpu_id: LogicalCpuId,
    name: String,
    pools: [&'static KtaskWorkerPool<MAX_WORKERS, ENTRY_CAP>; N],
    pool_name: PoolNameResolver,
    runtime_factory: WorkerRuntimeFactory<R>,
) -> bool
where
    R: WorkerRuntime<&'static KtaskWorkerPool<MAX_WORKERS, ENTRY_CAP>> + 'static,
{
    let manager_wake_source = Arc::new(PollEvent::new());
    let runtime = KtaskManagerRuntime::<R, MAX_WORKERS, ENTRY_CAP> {
        runtime_factory,
        pool_name,
        wake_source: manager_wake_source.clone(),
    };
    let task = ManagerTask::new(pools, runtime);
    let manager_task = prepare_bound_kthread(cpu_id, name, move || task.run());
    let thread_ref = WorkerThreadRef::new(manager_task.clone(), manager_wake_source);

    let mut installed = true;
    for pool in pools {
        installed &= pool.install_manager_thread(thread_ref.clone());
    }
    if installed {
        ktask::activate_task(&manager_task);
    }
    installed
}

fn wait_for_manager_work<const MAX_WORKERS: usize, const ENTRY_CAP: usize, const N: usize>(
    pools: &[&'static KtaskWorkerPool<MAX_WORKERS, ENTRY_CAP>; N],
    wake_source: &PollEvent,
) {
    let now = ktask::monotonic_time();
    let deadline = pools
        .iter()
        .filter_map(|pool| pool.next_management_deadline(now))
        .min();
    let _ = wait_for_event_until(wake_source, deadline, || {
        let now = ktask::monotonic_time();
        pools.iter().any(|pool| pool.manager_should_run(now))
    });
}

fn wait_for_event_until(
    wake_source: &PollEvent,
    deadline: Option<MonotonicInstant>,
    mut ready: impl FnMut() -> bool,
) -> bool {
    let mut registrations = PollRegistrations::new();
    loop {
        let observed_generation = wake_source.generation();
        let event = poll_fn(|cx| {
            if ready() {
                return Poll::Ready(true);
            }

            let mut context = registrations.context(cx);
            if wake_source.register(&mut context).is_err() {
                drop(context);
                return Poll::Ready(false);
            }
            drop(context);

            if ready() || wake_source.has_changed_since(observed_generation) {
                Poll::Ready(true)
            } else {
                Poll::Pending
            }
        });
        match ktask::future::block_on(ktask::future::timeout_at(deadline, event)) {
            Ok(true) => return true,
            Ok(false) => ktask::yield_now(),
            Err(_) => return false,
        }
    }
}

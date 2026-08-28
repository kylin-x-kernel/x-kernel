// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Built-in worker-pool instance and binding types.

use kcpu_id_map::LogicalCpuId;
use kspin::SpinNoIrqGuard;
use kworkerpool::{
    ActionBatch, EntryOwner as PoolEntryOwner, ImmediateAction, KtaskWorkerPool, PoolId, WorkerId,
    WorkerPool, WorkerPoolError, WorkerThreadRef,
};

use super::{
    entry::{pool_entry, pool_key, pool_owner},
    kind::{BUILTIN_POOL_ENTRY_CAP, BUILTIN_WORKERS_PER_POOL, SystemPoolKind, builtin_policy},
};

/// Core state stored inside one built-in pool instance.
pub(crate) type BuiltinPoolState =
    WorkerPool<WorkerThreadRef, { BUILTIN_WORKERS_PER_POOL }, { BUILTIN_POOL_ENTRY_CAP }>;

/// Ktask-backed runtime pool stored inside one built-in pool instance.
pub(crate) type BuiltinPoolRuntime =
    KtaskWorkerPool<{ BUILTIN_WORKERS_PER_POOL }, { BUILTIN_POOL_ENTRY_CAP }>;

/// One built-in system worker-pool instance.
pub(crate) struct BuiltinSystemWorkerPool {
    runtime: BuiltinPoolRuntime,
}

impl BuiltinSystemWorkerPool {
    pub(super) const fn new(kind: SystemPoolKind, cpu_id: LogicalCpuId) -> Self {
        Self {
            runtime: KtaskWorkerPool::new(
                PoolId::new(kind.pool_kind(), cpu_id),
                builtin_policy(kind),
            ),
        }
    }

    /// Returns this pool kind.
    pub(crate) fn kind(&self) -> SystemPoolKind {
        SystemPoolKind::from_usize(self.runtime.id().kind().as_usize())
            .expect("built-in worker-pool kind should be known")
    }

    /// Returns this pool CPU.
    pub(crate) fn cpu_id(&self) -> LogicalCpuId {
        self.runtime.id().cpu()
    }

    /// Returns this pool identity.
    pub(crate) fn id(&self) -> PoolId {
        self.runtime.id()
    }

    /// Returns whether this built-in pool has a runnable backend installed.
    pub(crate) fn is_ready(&self) -> bool {
        if self.kind().is_manager_managed() {
            return self.runtime.manager_thread_ref().is_some();
        }
        self.runtime.lock().installed_workers() != 0
    }

    /// Configures an empty const-initialized pool before workers are installed.
    pub(crate) fn configure_empty(&self, kind: SystemPoolKind, cpu_id: LogicalCpuId) -> bool {
        self.runtime
            .configure_empty(PoolId::new(kind.pool_kind(), cpu_id), builtin_policy(kind))
    }

    /// Returns the ktask-backed runtime pool.
    pub(crate) const fn runtime(&self) -> &BuiltinPoolRuntime {
        &self.runtime
    }

    /// Locks the worker-pool core state.
    pub(crate) fn lock(&self) -> SpinNoIrqGuard<'_, BuiltinPoolState> {
        self.runtime.lock()
    }

    /// Tries to lock the worker-pool core state without spinning.
    pub(crate) fn try_lock(&self) -> Option<SpinNoIrqGuard<'_, BuiltinPoolState>> {
        self.runtime.try_lock()
    }

    /// Installs a static worker slot for externally driven execution contexts.
    pub(crate) fn install_static_worker(&self, worker: WorkerId) -> bool {
        self.runtime
            .install_worker(worker, WorkerThreadRef::placeholder())
    }

    /// Wakes one installed worker.
    pub(crate) fn wake_worker(&self, worker: WorkerId) -> bool {
        self.runtime.wake_worker(worker)
    }

    /// Wakes this pool's manager thread.
    pub(crate) fn wake_manager(&self) -> bool {
        self.runtime.wake_manager()
    }
}

/// Opaque binding to one built-in system worker pool.
#[derive(Clone, Copy)]
pub(crate) struct SystemPoolBinding {
    pool: &'static BuiltinSystemWorkerPool,
}

impl SystemPoolBinding {
    pub(super) const fn new(pool: &'static BuiltinSystemWorkerPool) -> Self {
        Self { pool }
    }

    /// Returns the bound CPU.
    pub(crate) fn cpu_id(self) -> LogicalCpuId {
        self.pool.cpu_id()
    }

    /// Returns the bound pool object.
    pub(crate) const fn pool(self) -> &'static BuiltinSystemWorkerPool {
        self.pool
    }

    /// Applies one workqueue executor operation to this worker pool.
    pub(crate) fn apply_executor_op(
        self,
        op: kworkqueue::ExecutorOp,
    ) -> Result<(), BuiltinPoolEnqueueError> {
        let now = ktask::monotonic_time();
        let actions = match op {
            kworkqueue::ExecutorOp::EnqueueRunnable(entry) => self
                .pool()
                .lock()
                .enqueue_runnable(pool_entry(entry), now)?,
            kworkqueue::ExecutorOp::EnqueueInactive(entry) => {
                self.pool().lock().enqueue_deferred(pool_entry(entry))?;
                ActionBatch::new()
            }
            kworkqueue::ExecutorOp::Remove(entry) => {
                let _ = self
                    .pool()
                    .lock()
                    .remove_entry(pool_owner(entry.owner), pool_key(entry.key));
                ActionBatch::new()
            }
            kworkqueue::ExecutorOp::PromoteInactive { .. } => {
                return Err(BuiltinPoolEnqueueError::InvalidTransition);
            }
        };
        handle_actions(actions);
        Ok(())
    }

    pub(crate) fn promote_one_deferred_raw(
        self,
        owner: PoolEntryOwner,
        now: ktime_types::MonotonicInstant,
    ) -> Result<Option<(kworkqueue::ExecutorEntry, ActionBatch)>, BuiltinPoolEnqueueError> {
        let Some((entry, actions)) = self.pool().lock().promote_one_deferred(owner, now) else {
            return Ok(None);
        };
        let entry = super::entry::executor_entry(entry)
            .ok_or(BuiltinPoolEnqueueError::InvalidTransition)?;
        Ok(Some((entry, actions)))
    }

    pub(crate) fn has_executor_op_entry(self, op: kworkqueue::ExecutorOp) -> bool {
        let entry = match op {
            kworkqueue::ExecutorOp::EnqueueRunnable(entry)
            | kworkqueue::ExecutorOp::EnqueueInactive(entry)
            | kworkqueue::ExecutorOp::Remove(entry) => entry,
            kworkqueue::ExecutorOp::PromoteInactive { .. } => return false,
        };
        self.pool()
            .lock()
            .get_payload_mut(pool_owner(entry.owner), pool_key(entry.key))
            .is_some()
    }
}

pub(crate) fn handle_actions(actions: ActionBatch) {
    for action in actions.immediate() {
        match action {
            ImmediateAction::WakeWorker { pool, worker } => {
                if let Some(kind) = SystemPoolKind::from_usize(pool.kind().as_usize()) {
                    let _ = super::system::wake_system_worker(kind, pool.cpu(), worker);
                }
            }
            ImmediateAction::WakeManager { pool } => {
                if let Some(kind) = SystemPoolKind::from_usize(pool.kind().as_usize()) {
                    let _ = super::system::wake_system_manager(kind, pool.cpu());
                }
            }
            ImmediateAction::RaiseBottomHalf { .. } => {
                // A generic worker-pool action does not know which BH workqueue
                // kind produced the runnable entry. kwork raises the exact
                // softirq in enqueue, promote, and drain paths while it still
                // has the BottomHalfWorkQueueKind.
            }
            ImmediateAction::ArmCpuIntensiveTimer { .. } => {}
        }
    }
}

/// Error returned when a workqueue executor operation cannot be applied.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BuiltinPoolEnqueueError {
    QueueFull,
    InvalidWorker,
    InvalidTransition,
}

impl From<WorkerPoolError> for BuiltinPoolEnqueueError {
    fn from(error: WorkerPoolError) -> Self {
        match error {
            WorkerPoolError::QueueFull(_) => Self::QueueFull,
            WorkerPoolError::InvalidWorker => Self::InvalidWorker,
            _ => Self::InvalidTransition,
        }
    }
}

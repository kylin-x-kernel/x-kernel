// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Static built-in worker-pool set.

use kcpu_id_map::LogicalCpuId;
use ktime_types::MonotonicInstant;
use kworkerpool::{PoolId, WorkerId, WorkerRuntime, WorkerRuntimeFactory, decode_task_context};

use super::{
    cpu::{BuiltinCpuPoolInitResult, BuiltinCpuWorkerPools},
    kind::SystemPoolKind,
    pool::{BuiltinPoolRuntime, BuiltinSystemWorkerPool, SystemPoolBinding},
};

/// Built-in per-CPU system worker-pool set.
pub(crate) struct BuiltinSystemWorkerPools {
    cpus: [BuiltinCpuWorkerPools; kbuild_config::NR_CPUS],
}

impl BuiltinSystemWorkerPools {
    const fn new() -> Self {
        Self { cpus: make_cpus() }
    }

    /// Returns one CPU's built-in worker pools.
    pub(crate) fn cpu(
        &'static self,
        cpu_id: LogicalCpuId,
    ) -> Option<&'static BuiltinCpuWorkerPools> {
        self.cpus.get(cpu_id.as_usize())
    }

    /// Returns one built-in worker pool selected by kind and CPU.
    pub(crate) fn pool(
        &'static self,
        kind: SystemPoolKind,
        cpu_id: LogicalCpuId,
    ) -> Option<&'static BuiltinSystemWorkerPool> {
        let cpu = self.cpu(cpu_id)?;
        match kind {
            SystemPoolKind::Normal => Some(cpu.normal()),
            SystemPoolKind::Bh => Some(cpu.bh()),
        }
    }

    /// Resolves a pool by kind and CPU.
    pub(crate) fn for_kind_cpu(
        &'static self,
        kind: SystemPoolKind,
        cpu_id: LogicalCpuId,
    ) -> Option<SystemPoolBinding> {
        let pool = self.pool(kind, cpu_id)?;
        pool.is_ready().then_some(SystemPoolBinding::new(pool))
    }

    /// Resolves the normal system pool for one CPU.
    pub(crate) fn for_cpu(&'static self, cpu_id: LogicalCpuId) -> Option<SystemPoolBinding> {
        self.for_kind_cpu(SystemPoolKind::Normal, cpu_id)
    }

    /// Initializes this CPU's built-in worker pools.
    pub(crate) fn init_cpu<R>(
        &'static self,
        cpu_id: LogicalCpuId,
        runtime_factory: WorkerRuntimeFactory<R>,
    ) -> Option<BuiltinCpuPoolInitResult>
    where
        R: WorkerRuntime<&'static BuiltinPoolRuntime> + 'static,
    {
        let cpu = self.cpu(cpu_id)?;
        cpu.configure(cpu_id).then_some(())?;
        Some(cpu.init(cpu_id, runtime_factory))
    }

    /// Returns whether the selected built-in pool exists with the expected id.
    pub(crate) fn is_pool_ready(&'static self, kind: SystemPoolKind, cpu_id: LogicalCpuId) -> bool {
        self.pool(kind, cpu_id).is_some_and(|pool| {
            pool.id() == PoolId::new(kind.pool_kind(), cpu_id) && pool.is_ready()
        })
    }

    /// Wakes one worker thread in a built-in pool.
    pub(crate) fn wake_worker(
        &'static self,
        kind: SystemPoolKind,
        cpu_id: LogicalCpuId,
        worker: WorkerId,
    ) -> bool {
        self.pool(kind, cpu_id)
            .is_some_and(|pool| pool.wake_worker(worker))
    }

    /// Wakes the manager thread for a built-in task-context pool.
    pub(crate) fn wake_manager(&'static self, kind: SystemPoolKind, cpu_id: LogicalCpuId) -> bool {
        if !kind.is_manager_managed() {
            return false;
        }
        self.pool(kind, cpu_id)
            .is_some_and(|pool| pool.wake_manager())
    }
}

static SYSTEM_WORKER_POOLS: BuiltinSystemWorkerPools = BuiltinSystemWorkerPools::new();

/// Returns the global built-in worker-pool set.
pub(crate) const fn system_pools() -> &'static BuiltinSystemWorkerPools {
    &SYSTEM_WORKER_POOLS
}

/// Initializes the current CPU's built-in system worker pools.
pub(crate) fn init_system_worker_pools_for_cpu<R>(
    cpu_id: LogicalCpuId,
    runtime_factory: WorkerRuntimeFactory<R>,
) -> Option<BuiltinCpuPoolInitResult>
where
    R: WorkerRuntime<&'static BuiltinPoolRuntime> + 'static,
{
    system_pools().init_cpu(cpu_id, runtime_factory)
}

/// Returns whether the built-in pool for `(kind, cpu_id)` has been initialized.
pub(crate) fn is_system_worker_pool_ready(kind: SystemPoolKind, cpu_id: LogicalCpuId) -> bool {
    system_pools().is_pool_ready(kind, cpu_id)
}

/// Resolves the normal system pool for one CPU.
pub(crate) fn system_pool_for_cpu(cpu_id: LogicalCpuId) -> Option<SystemPoolBinding> {
    system_pools().for_cpu(cpu_id)
}

/// Resolves a built-in system pool by kind and CPU.
pub(crate) fn system_pool_for_kind_cpu(
    kind: SystemPoolKind,
    cpu_id: LogicalCpuId,
) -> Option<SystemPoolBinding> {
    system_pools().for_kind_cpu(kind, cpu_id)
}

/// Wakes one runtime worker thread owned by a built-in pool, if available.
pub(crate) fn wake_system_worker(
    kind: SystemPoolKind,
    cpu_id: LogicalCpuId,
    worker: WorkerId,
) -> bool {
    system_pools().wake_worker(kind, cpu_id, worker)
}

/// Wakes the per-CPU manager thread owned by a built-in pool.
pub(crate) fn wake_system_manager(kind: SystemPoolKind, cpu_id: LogicalCpuId) -> bool {
    system_pools().wake_manager(kind, cpu_id)
}

/// Accounts one scheduler tick for a task currently running built-in pool work.
pub(crate) fn account_system_execution_tick(
    context: ktask::TaskExecutionContext,
    now: MonotonicInstant,
) -> Option<MonotonicInstant> {
    let decoded = decode_task_context(context)?;
    let kind = SystemPoolKind::from_usize(decoded.pool_id.kind().as_usize())?;
    let pool = system_pools().pool(kind, decoded.pool_id.cpu())?;
    let result = pool.runtime().account_execution_tick(context, now)?;
    super::pool::handle_actions(result.actions);
    result.deadline
}

/// Returns the next CPU-intensive tick deadline for a built-in pool execution.
pub(crate) fn system_execution_tick_deadline(
    context: ktask::TaskExecutionContext,
) -> Option<MonotonicInstant> {
    let decoded = decode_task_context(context)?;
    let kind = SystemPoolKind::from_usize(decoded.pool_id.kind().as_usize())?;
    let pool = system_pools().pool(kind, decoded.pool_id.cpu())?;
    pool.runtime().execution_tick_deadline(context)
}

const fn make_cpus() -> [BuiltinCpuWorkerPools; kbuild_config::NR_CPUS] {
    [const { BuiltinCpuWorkerPools::new() }; kbuild_config::NR_CPUS]
}

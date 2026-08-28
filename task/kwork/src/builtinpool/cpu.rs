// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Built-in worker pools belonging to one CPU.

use alloc::string::String;

use kcpu_id_map::LogicalCpuId;
use kworkerpool::{
    PoolId, PoolNameResolver, WorkerId, WorkerRuntime, WorkerRuntimeFactory, manager_name,
    start_manager_task,
};

use super::{
    kind::{SystemPoolKind, builtin_policy},
    pool::{BuiltinPoolRuntime, BuiltinSystemWorkerPool},
};

/// Built-in worker-pool initialization result for one CPU.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BuiltinCpuPoolInitResult {
    /// Task-context normal pool initialization result.
    pub managed: BuiltinWorkerPoolInitResult,
    /// Bottom-half pool initialization result.
    pub bh: BuiltinBhPoolInitResult,
}

/// Result of built-in managed worker-pool initialization for one CPU.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BuiltinWorkerPoolInitResult {
    /// Number of initial worker threads successfully started.
    pub initial_workers_started: usize,
    /// Whether the per-CPU manager thread was started.
    pub manager_started: bool,
}

/// Result of built-in bottom-half worker-pool initialization for one CPU.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BuiltinBhPoolInitResult {
    /// Number of static drain worker slots installed.
    pub static_workers_installed: usize,
}

/// Built-in worker pools belonging to one CPU.
///
/// Normal work uses ktask-backed worker and manager threads. Bottom-half work
/// is represented as pool state here, while execution is driven by the softirq
/// drain context rather than by a ktask worker loop.
pub(crate) struct BuiltinCpuWorkerPools {
    normal: BuiltinSystemWorkerPool,
    bh: BuiltinSystemWorkerPool,
}

impl BuiltinCpuWorkerPools {
    /// Creates a const-initialized CPU pool group.
    pub(crate) const fn new() -> Self {
        Self {
            normal: BuiltinSystemWorkerPool::new(SystemPoolKind::Normal, LogicalCpuId::new(0)),
            bh: BuiltinSystemWorkerPool::new(SystemPoolKind::Bh, LogicalCpuId::new(0)),
        }
    }

    /// Configures this CPU's pool identities before workers are installed.
    pub(crate) fn configure(&self, cpu_id: LogicalCpuId) -> bool {
        self.normal.configure_empty(SystemPoolKind::Normal, cpu_id)
            && self.bh.configure_empty(SystemPoolKind::Bh, cpu_id)
    }

    /// Returns the task-context normal pool.
    pub(crate) fn normal(&'static self) -> &'static BuiltinSystemWorkerPool {
        &self.normal
    }

    /// Returns the bottom-half pool.
    pub(crate) fn bh(&'static self) -> &'static BuiltinSystemWorkerPool {
        &self.bh
    }

    /// Initializes both built-in pools for this CPU.
    pub(crate) fn init<R>(
        &'static self,
        cpu_id: LogicalCpuId,
        runtime_factory: WorkerRuntimeFactory<R>,
    ) -> BuiltinCpuPoolInitResult
    where
        R: WorkerRuntime<&'static BuiltinPoolRuntime> + 'static,
    {
        BuiltinCpuPoolInitResult {
            managed: self.init_normal(cpu_id, runtime_factory),
            bh: self.init_bh(),
        }
    }

    fn init_normal<R>(
        &'static self,
        cpu_id: LogicalCpuId,
        runtime_factory: WorkerRuntimeFactory<R>,
    ) -> BuiltinWorkerPoolInitResult
    where
        R: WorkerRuntime<&'static BuiltinPoolRuntime> + 'static,
    {
        let policy = builtin_policy(SystemPoolKind::Normal);
        let initial_workers = policy.initial_workers().min(policy.max_workers());
        let mut result = BuiltinWorkerPoolInitResult::default();
        let pool = self.normal.runtime();

        for worker_id in 0..initial_workers {
            if pool.start_worker_task(
                WorkerId::new(worker_id),
                SystemPoolKind::Normal.pool_name(),
                runtime_factory,
            ) {
                result.initial_workers_started += 1;
            }
        }

        result.manager_started = start_manager_task(
            cpu_id,
            manager_task_name(SystemPoolKind::Normal, cpu_id),
            [pool],
            pool_name,
            runtime_factory,
        );
        result
    }

    fn init_bh(&'static self) -> BuiltinBhPoolInitResult {
        let policy = builtin_policy(SystemPoolKind::Bh);
        let initial_workers = policy.initial_workers().min(policy.max_workers());
        let mut result = BuiltinBhPoolInitResult::default();

        for worker_id in 0..initial_workers {
            if self.bh.install_static_worker(WorkerId::new(worker_id)) {
                result.static_workers_installed += 1;
            }
        }
        result
    }
}

const fn pool_name(pool_id: PoolId) -> &'static str {
    match SystemPoolKind::from_usize(pool_id.kind().as_usize()) {
        Some(kind) => kind.pool_name(),
        None => "unknown",
    }
}

fn manager_task_name(kind: SystemPoolKind, cpu_id: LogicalCpuId) -> String {
    manager_name(PoolId::new(kind.pool_kind(), cpu_id), kind.pool_name())
}

#[allow(dead_code)]
fn _assert_manager_pool_name_resolver(_: PoolNameResolver) {}

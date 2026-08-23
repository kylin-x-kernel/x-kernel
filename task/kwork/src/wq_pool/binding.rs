// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kcpu_id_map::LogicalCpuId;

use crate::{
    BottomHalfPoolBinding, BottomHalfWake, TaskPoolBinding, TaskPoolWake, WorkerPool,
    WorkerWakePlan,
};

/// Runtime binding needed by one `WorkQueuePoolBinding`.
///
/// Linux `pool_workqueue` directly stores `pool` and is reached through
/// `wq->cpu_pwq[cpu]`. X-Kernel keeps the same semantic split: `pool` is the
/// execution pool and `cpu_id` selects the queue-owned per-CPU binding state. The
/// provider-facing system lane is hidden behind [`TaskPoolBinding`] so system
/// queue names do not become core binding state.
#[derive(Clone, Copy)]
pub(super) struct PoolRuntimeBinding {
    execution_pool: ExecutionPoolBinding,
}

/// Runtime execution binding for one generic `pool_workqueue`.
#[derive(Clone, Copy)]
pub(super) enum ExecutionPoolBinding {
    Task(TaskPoolBinding),
    BottomHalf(BottomHalfPoolBinding),
    #[cfg(unittest)]
    Test {
        pool: &'static WorkerPool,
        cpu_id: LogicalCpuId,
    },
}

/// Wake plan paired with the runtime target that can execute it.
///
/// Core accounting returns a [`WorkerWakePlan`]; only the runtime binding knows
/// which provider-visible worker pool should receive that plan. Keeping the
/// pair in one type avoids passing raw `(kind, cpu, plan)` tuples through binding
/// accounting code.
#[derive(Clone, Copy)]
pub(crate) struct PoolWake {
    wake: ExecutionPoolWake,
}

#[derive(Clone, Copy)]
enum ExecutionPoolWake {
    Task(TaskPoolWake),
    BottomHalf(BottomHalfWake),
    #[cfg(unittest)]
    None,
}

impl PoolRuntimeBinding {
    pub(super) fn new(execution_pool: ExecutionPoolBinding) -> Self {
        Self { execution_pool }
    }

    pub(super) fn pool_key(self) -> usize {
        self.execution_pool.pool_key()
    }

    pub(super) fn cpu_id(self) -> LogicalCpuId {
        self.execution_pool.cpu_id()
    }

    pub(super) fn pool(self) -> &'static WorkerPool {
        self.execution_pool.pool()
    }

    pub(super) fn wake(self, plan: WorkerWakePlan) -> PoolWake {
        PoolWake {
            wake: self.execution_pool.wake(plan),
        }
    }
}

impl PoolWake {
    pub(crate) fn execute(self) {
        self.wake.execute();
    }
}

impl ExecutionPoolBinding {
    pub(super) fn cpu_id(self) -> LogicalCpuId {
        match self {
            Self::Task(binding) => binding.cpu_id(),
            Self::BottomHalf(binding) => binding.cpu_id(),
            #[cfg(unittest)]
            Self::Test { cpu_id, .. } => cpu_id,
        }
    }

    pub(super) fn pool(self) -> &'static WorkerPool {
        match self {
            Self::Task(binding) => binding.pool(),
            Self::BottomHalf(binding) => binding.pool(),
            #[cfg(unittest)]
            Self::Test { pool, .. } => pool,
        }
    }

    fn pool_key(self) -> usize {
        self.pool().key()
    }

    fn wake(self, plan: WorkerWakePlan) -> ExecutionPoolWake {
        match self {
            Self::Task(binding) => ExecutionPoolWake::Task(binding.wake(plan)),
            Self::BottomHalf(binding) => ExecutionPoolWake::BottomHalf(binding.wake(plan)),
            #[cfg(unittest)]
            Self::Test { .. } => {
                let _ = plan;
                ExecutionPoolWake::None
            }
        }
    }
}

impl ExecutionPoolWake {
    fn execute(self) {
        match self {
            Self::Task(wake) => wake.execute(),
            Self::BottomHalf(wake) => wake.execute(),
            #[cfg(unittest)]
            Self::None => {}
        }
    }
}

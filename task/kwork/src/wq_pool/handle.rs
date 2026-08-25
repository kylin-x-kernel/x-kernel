// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kcpu_id_map::LogicalCpuId;

use super::{
    accounting::WorkQueuePoolState,
    binding::{ExecutionPoolBinding, PoolRuntimeBinding},
};
#[cfg(unittest)]
use crate::WorkerPool;
use crate::{
    BottomHalfPoolBinding, QueueOwner, QueueWorkResult, TaskPoolBinding, WorkQueue,
    WorkQueueHandle, WorkqueueHostIf, bh_wq_kind,
};

/// Runtime policy that resolves one logical workqueue onto execution pools.
pub(crate) trait WorkQueueRuntime {
    fn select_pool_binding(
        self,
        cpu_id: Option<LogicalCpuId>,
    ) -> Result<WorkQueuePoolBinding, QueueWorkResult>;

    fn all_pool_bindings(self) -> Result<alloc::vec::Vec<WorkQueuePoolBinding>, QueueWorkResult>;
}

/// Resolved binding between a logical workqueue and an execution resource.
///
/// This is the X-Kernel counterpart of Linux `pool_workqueue`.
///
/// It resolves one `(workqueue, worker_pool)` relationship and owns the
/// corresponding [`WorkQueuePoolState`] accounting boundary. Pending entries
/// live in the pool's shared fixed-capacity ring; active limits, colors, flush
/// state, and in-flight counters remain local to this binding.
#[derive(Clone)]
pub(crate) struct WorkQueuePoolBinding {
    pub(super) owner: QueueOwner,
    pub(super) binding: PoolRuntimeBinding,
}

impl WorkQueuePoolBinding {
    #[cfg(unittest)]
    pub(crate) fn for_static(queue: &'static WorkQueue) -> Result<Self, QueueWorkResult> {
        queue.select_pool_binding(None)
    }

    #[cfg(unittest)]
    pub(crate) fn for_all_owner_cpus(
        owner: QueueOwner,
    ) -> Result<alloc::vec::Vec<Self>, QueueWorkResult> {
        match owner {
            QueueOwner::Static(queue) => queue.all_pool_bindings(),
            QueueOwner::Dynamic(queue) => queue.all_pool_bindings(),
        }
    }

    pub(crate) fn owner(&self) -> QueueOwner {
        self.owner.clone()
    }

    pub(crate) fn pool_key(&self) -> usize {
        self.binding.pool_key()
    }

    pub(crate) fn binding_key(&self) -> usize {
        self.owner.queue().key()
    }

    pub(crate) fn same_binding(&self, other: &Self) -> bool {
        self.pool_key() == other.pool_key() && self.owner().same_queue(other.owner().queue())
    }

    pub(crate) fn state(&self) -> &kspin::SpinNoIrq<WorkQueuePoolState> {
        self.owner
            .queue()
            .pool_state_for_cpu(self.binding.cpu_id())
            .expect("resolved workqueue-pool binding CPU should remain valid")
    }

    pub(super) fn resolved(owner: QueueOwner, binding: ExecutionPoolBinding) -> Self {
        Self {
            owner,
            binding: PoolRuntimeBinding::new(binding),
        }
    }

    #[cfg(unittest)]
    pub(crate) fn for_test_pool(owner: QueueOwner, pool: &'static WorkerPool) -> Self {
        Self {
            owner,
            binding: PoolRuntimeBinding::new(ExecutionPoolBinding::Test {
                pool,
                cpu_id: WorkqueueHostIf::current_cpu_id(),
            }),
        }
    }
}

fn default_pool_binding_for_owner_cpu(
    owner: QueueOwner,
    cpu_id: LogicalCpuId,
) -> Result<WorkQueuePoolBinding, QueueWorkResult> {
    let Some(binding) = TaskPoolBinding::default_for_cpu(cpu_id) else {
        return Err(default_pool_cpu_error(cpu_id));
    };
    if owner.queue().pool_state_for_cpu(cpu_id).is_none() {
        return Err(QueueWorkResult::InvalidCpu);
    }
    Ok(WorkQueuePoolBinding {
        owner,
        binding: PoolRuntimeBinding::new(ExecutionPoolBinding::Task(binding)),
    })
}

fn default_pool_cpu_error(cpu_id: LogicalCpuId) -> QueueWorkResult {
    if cpu_id.as_usize() >= kbuild_config::NR_CPUS {
        QueueWorkResult::InvalidCpu
    } else {
        QueueWorkResult::WorkerUnavailable
    }
}

fn static_queue_cpu_error(queue: &'static WorkQueue, cpu_id: LogicalCpuId) -> QueueWorkResult {
    if queue.pool_state_for_cpu(cpu_id).is_none() {
        QueueWorkResult::InvalidCpu
    } else {
        QueueWorkResult::WorkerUnavailable
    }
}

impl WorkQueueRuntime for &'static WorkQueue {
    fn select_pool_binding(
        self,
        cpu_id: Option<LogicalCpuId>,
    ) -> Result<WorkQueuePoolBinding, QueueWorkResult> {
        let owner = QueueOwner::Static(self);
        let target_cpu = cpu_id.unwrap_or_else(WorkqueueHostIf::current_cpu_id);
        if let Some(kind) = bh_wq_kind(self) {
            let Some(binding) = BottomHalfPoolBinding::for_kind_cpu(kind, target_cpu)
                .filter(|binding| core::ptr::eq(binding.queue(), self))
            else {
                return Err(static_queue_cpu_error(self, target_cpu));
            };
            return Ok(WorkQueuePoolBinding {
                owner,
                binding: PoolRuntimeBinding::new(ExecutionPoolBinding::BottomHalf(binding)),
            });
        }
        let Some(binding) = TaskPoolBinding::for_static_queue_cpu(self, target_cpu) else {
            return Err(static_queue_cpu_error(self, target_cpu));
        };
        Ok(WorkQueuePoolBinding {
            owner,
            binding: PoolRuntimeBinding::new(ExecutionPoolBinding::Task(binding)),
        })
    }

    fn all_pool_bindings(self) -> Result<alloc::vec::Vec<WorkQueuePoolBinding>, QueueWorkResult> {
        if let Some(kind) = bh_wq_kind(self) {
            let mut bindings = alloc::vec::Vec::with_capacity(kbuild_config::NR_CPUS);
            for cpu_index in 0..kbuild_config::NR_CPUS {
                let cpu_id = LogicalCpuId::new(cpu_index);
                let Some(binding) = BottomHalfPoolBinding::for_kind_cpu(kind, cpu_id)
                    .filter(|binding| core::ptr::eq(binding.queue(), self))
                else {
                    return Err(static_queue_cpu_error(self, cpu_id));
                };
                bindings.push(WorkQueuePoolBinding {
                    owner: QueueOwner::Static(self),
                    binding: PoolRuntimeBinding::new(ExecutionPoolBinding::BottomHalf(binding)),
                });
            }
            return Ok(bindings);
        }

        let mut bindings = alloc::vec::Vec::with_capacity(kbuild_config::NR_CPUS);
        for cpu_index in 0..kbuild_config::NR_CPUS {
            bindings.push(default_pool_binding_for_owner_cpu(
                QueueOwner::Static(self),
                LogicalCpuId::new(cpu_index),
            )?);
        }
        Ok(bindings)
    }
}

impl WorkQueueRuntime for WorkQueueHandle {
    fn select_pool_binding(
        self,
        cpu_id: Option<LogicalCpuId>,
    ) -> Result<WorkQueuePoolBinding, QueueWorkResult> {
        default_pool_binding_for_owner_cpu(
            QueueOwner::Dynamic(self),
            cpu_id.unwrap_or_else(WorkqueueHostIf::current_cpu_id),
        )
    }

    fn all_pool_bindings(self) -> Result<alloc::vec::Vec<WorkQueuePoolBinding>, QueueWorkResult> {
        let mut bindings = alloc::vec::Vec::with_capacity(kbuild_config::NR_CPUS);
        for cpu_index in 0..kbuild_config::NR_CPUS {
            bindings.push(default_pool_binding_for_owner_cpu(
                QueueOwner::Dynamic(self.clone()),
                LogicalCpuId::new(cpu_index),
            )?);
        }
        Ok(bindings)
    }
}

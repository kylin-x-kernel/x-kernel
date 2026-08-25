// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kcpu_id_map::LogicalCpuId;

use crate::{
    WorkQueue, WorkerPool, WorkerPoolAttrs, WorkerPoolCpuAffinity, WorkerPoolExecution,
    WorkerPoolSchedulingPolicy, WorkerWakePlan, WorkqueueBottomHalfIf,
};

const BH_DRAIN_SLOT_ID: usize = 0;

/// Built-in bottom-half workerqueue kind.
///
/// This mirrors Linux's `system_bh_wq` and `system_bh_highpri_wq` instance
/// family. The queue instances are runtime objects; the bottom-half execution
/// context is owned by the drain side, not by the `ScheduledWork` object.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BottomHalfWorkQueueKind {
    /// Default bottom-half queue for softirq-style deferred work.
    Default,
    /// High-priority bottom-half queue.
    HighPri,
}

impl BottomHalfWorkQueueKind {
    /// All built-in bottom-half queue kinds.
    pub const ALL: [Self; Self::COUNT] = [Self::Default, Self::HighPri];
    /// Number of built-in bottom-half queue kinds.
    pub const COUNT: usize = 2;

    /// Returns the stable index used by runtime per-kind arrays.
    pub const fn as_usize(self) -> usize {
        match self {
            Self::Default => 0,
            Self::HighPri => 1,
        }
    }

    /// Returns the built-in queue name.
    pub const fn queue_name(self) -> &'static str {
        match self {
            Self::Default => "system_bh_wq",
            Self::HighPri => "system_bh_highpri_wq",
        }
    }

    pub(crate) const fn scheduling_policy(self) -> WorkerPoolSchedulingPolicy {
        match self {
            Self::Default => WorkerPoolSchedulingPolicy::Normal,
            Self::HighPri => WorkerPoolSchedulingPolicy::HighPriority,
        }
    }
}

static BH_WORKQUEUES: [WorkQueue; BottomHalfWorkQueueKind::COUNT] = [
    WorkQueue::new("system_bh_wq"),
    WorkQueue::new("system_bh_highpri_wq"),
];

static BH_POOLS: [[WorkerPool; kbuild_config::NR_CPUS]; BottomHalfWorkQueueKind::COUNT] = [
    [const { WorkerPool::new() }; kbuild_config::NR_CPUS],
    [const { WorkerPool::new() }; kbuild_config::NR_CPUS],
];

/// Handle to one per-CPU bottom-half pool binding.
///
/// This binding is the runtime view over a bottom-half [`WorkQueue`], its
/// per-CPU [`WorkerPool`] execution state. The queue-owned binding accounting is
/// resolved by `WorkQueuePoolBinding` from `(queue, cpu)`, so this binding carries only
/// the fields the bottom-half provider needs to raise and drain work.
#[derive(Clone, Copy)]
pub struct BottomHalfPoolBinding {
    kind: BottomHalfWorkQueueKind,
    cpu_id: LogicalCpuId,
    queue: &'static WorkQueue,
    pool: &'static WorkerPool,
}

impl BottomHalfPoolBinding {
    fn new(
        kind: BottomHalfWorkQueueKind,
        cpu_id: LogicalCpuId,
        queue: &'static WorkQueue,
        pool: &'static WorkerPool,
    ) -> Self {
        pool.ensure_attrs(WorkerPoolAttrs::new(
            WorkerPoolExecution::BottomHalf,
            kind.scheduling_policy(),
            WorkerPoolCpuAffinity::Pinned(cpu_id),
        ));
        Self {
            kind,
            cpu_id,
            queue,
            pool,
        }
    }

    /// Resolves the bottom-half pool binding for one queue kind and CPU.
    ///
    /// Returns `None` if `cpu_id` is outside `NR_CPUS`.
    pub fn for_kind_cpu(kind: BottomHalfWorkQueueKind, cpu_id: LogicalCpuId) -> Option<Self> {
        let binding = Self::new(
            kind,
            cpu_id,
            bh_queue_cpu_is_valid(cpu_id).then(|| bh_wq_for_kind(kind))?,
            bh_pool_for_kind_cpu(kind, cpu_id)?,
        );
        binding.ensure_pseudo_worker();
        Some(binding)
    }

    /// Returns the bottom-half queue kind of this pool.
    pub fn kind(self) -> BottomHalfWorkQueueKind {
        self.kind
    }

    /// Returns the logical CPU this pool is bound to.
    pub fn cpu_id(self) -> LogicalCpuId {
        self.cpu_id
    }

    /// Returns the bottom-half workqueue bound to this pool.
    pub fn queue(self) -> &'static WorkQueue {
        self.queue
    }

    pub(crate) fn pool(self) -> &'static WorkerPool {
        self.pool
    }

    /// Returns the execution attributes of this bottom-half worker pool.
    pub fn attrs(self) -> WorkerPoolAttrs {
        self.pool
            .attrs()
            .expect("bottom-half pool binding should initialize worker-pool attrs")
    }

    /// Returns whether this bottom-half pool has runnable work.
    pub fn has_runnable_work(self) -> bool {
        self.pool.state.lock().has_runnable_work()
    }

    pub(crate) fn wake(self, plan: WorkerWakePlan) -> BottomHalfWake {
        BottomHalfWake {
            kind: self.kind,
            should_raise: plan.worker_to_wake.is_some() || plan.should_wake_manager,
        }
    }

    fn ensure_pseudo_worker(self) {
        let _ = self.pool.state.lock().install_worker(BH_DRAIN_SLOT_ID);
    }
}

/// Deferred wake request for a bottom-half execution pool.
#[derive(Clone, Copy)]
pub(crate) struct BottomHalfWake {
    kind: BottomHalfWorkQueueKind,
    should_raise: bool,
}

impl BottomHalfWake {
    pub(crate) fn execute(self) {
        if self.should_raise {
            WorkqueueBottomHalfIf::raise_bottom_half(self.kind);
        }
    }
}

/// Returns the default global bottom-half system workqueue.
///
/// This is the X-Kernel runtime counterpart of Linux `system_bh_wq`.
pub fn system_bh_wq() -> &'static WorkQueue {
    bh_wq_for_kind(BottomHalfWorkQueueKind::Default)
}

/// Returns the global high-priority bottom-half system workqueue.
pub fn system_bh_highpri_wq() -> &'static WorkQueue {
    bh_wq_for_kind(BottomHalfWorkQueueKind::HighPri)
}

pub(crate) fn bh_wq_kind(queue: &'static WorkQueue) -> Option<BottomHalfWorkQueueKind> {
    BottomHalfWorkQueueKind::ALL
        .into_iter()
        .find(|&kind| core::ptr::eq(&BH_WORKQUEUES[kind.as_usize()], queue))
}

pub(crate) fn bh_wq_for_kind(kind: BottomHalfWorkQueueKind) -> &'static WorkQueue {
    &BH_WORKQUEUES[kind.as_usize()]
}

pub(crate) fn bh_queue_cpu_is_valid(cpu_id: LogicalCpuId) -> bool {
    cpu_id.as_usize() < kbuild_config::NR_CPUS
}

fn bh_pool_for_kind_cpu(
    kind: BottomHalfWorkQueueKind,
    cpu_id: LogicalCpuId,
) -> Option<&'static WorkerPool> {
    BH_POOLS
        .get(kind.as_usize())
        .and_then(|pools| pools.get(cpu_id.as_usize()))
}

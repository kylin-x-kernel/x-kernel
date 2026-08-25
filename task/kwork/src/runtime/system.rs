// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kcpu_id_map::LogicalCpuId;

mod pools;
mod queues;

pub use pools::{
    INITIAL_SYSTEM_WORKERS_PER_CPU, MAX_SYSTEM_WORKERS_PER_CPU, SystemPoolBinding, SystemPoolKind,
};
pub(crate) use pools::{TaskPoolBinding, TaskPoolWake};
pub use queues::SystemWorkQueueKind;

use self::{pools::SystemWorkerPools, queues::SystemWorkQueues};
use crate::{
    QueueWorkResult, ScheduledWork, WorkQueue, WorkQueueRuntime, WorkerPool, WorkerWakePlan,
    finish_workqueue_pool_enqueue,
};

/// Returns the default global bound system workqueue.
///
/// This is the X-Kernel runtime counterpart of Linux `system_percpu_wq`, the
/// queue used by `schedule_work*()` and `schedule_delayed_work*()` helpers.
/// The queue object is global; enqueue-time CPU selection resolves the
/// corresponding per-CPU pool-workqueue binding.
pub fn system_percpu_wq() -> &'static WorkQueue {
    system_wq_for_kind(SystemWorkQueueKind::Default)
}

/// Returns the default global bound system workqueue.
///
/// Compatibility alias for [`system_percpu_wq`]. New code should use
/// [`system_percpu_wq`] to match Linux, where `system_wq` is a deprecated
/// compatibility instance and `schedule_work*()` targets `system_percpu_wq`.
pub fn system_wq() -> &'static WorkQueue {
    system_percpu_wq()
}

/// Returns the global long-running bound system workqueue.
///
/// Long system work has a separate queue and flush/accounting domain from
/// [`system_wq`], but shares the same default per-CPU worker pool.
pub fn system_long_wq() -> &'static WorkQueue {
    system_wq_for_kind(SystemWorkQueueKind::Long)
}

/// Queues `work` on the current CPU's default system workerqueue.
pub fn schedule_work(work: &ScheduledWork) -> QueueWorkResult {
    system_percpu_wq().queue_work(work)
}

/// Queues `work` on the default system workerqueue bound to `cpu_id`.
pub fn schedule_work_on(cpu_id: LogicalCpuId, work: &ScheduledWork) -> QueueWorkResult {
    let queue = system_percpu_wq();
    match queue.select_pool_binding(Some(cpu_id)) {
        Ok(binding) => finish_workqueue_pool_enqueue(binding.queue_work(work)),
        Err(result) => result,
    }
}

/// Queues `work` on the current CPU's long-running system workerqueue.
pub fn schedule_long_work(work: &ScheduledWork) -> QueueWorkResult {
    system_long_wq().queue_work(work)
}

/// Queues `work` on the long-running system workerqueue bound to `cpu_id`.
pub fn schedule_long_work_on(cpu_id: LogicalCpuId, work: &ScheduledWork) -> QueueWorkResult {
    let queue = system_long_wq();
    match queue.select_pool_binding(Some(cpu_id)) {
        Ok(binding) => finish_workqueue_pool_enqueue(binding.queue_work(work)),
        Err(result) => result,
    }
}

pub(crate) fn system_wq_for_kind(kind: SystemWorkQueueKind) -> &'static WorkQueue {
    SystemWorkQueues::for_kind(kind)
}

pub(crate) fn system_queue_cpu_is_valid(cpu_id: LogicalCpuId) -> bool {
    SystemWorkQueues::cpu_is_valid(cpu_id)
}

pub(crate) fn system_wq_kind(queue: &'static WorkQueue) -> Option<SystemWorkQueueKind> {
    SystemWorkQueues::kind(queue)
}

pub(crate) fn system_pool_for_cpu(cpu_id: LogicalCpuId) -> Option<&'static WorkerPool> {
    SystemWorkerPools::for_cpu(cpu_id)
}

pub(crate) fn wake_system_pool_plan(
    pool_kind: SystemPoolKind,
    cpu_id: LogicalCpuId,
    plan: WorkerWakePlan,
) {
    SystemWorkerPools::wake_plan(pool_kind, cpu_id, plan);
}

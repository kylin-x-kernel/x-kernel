// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Affinity updates and cross-RQ migrate-in.

use khal::percpu::this_cpu_id;

#[cfg(feature = "smp")]
use super::rq::request_remote_resched;
#[cfg(feature = "smp")]
use super::stats::record_remote_resched;
use super::{
    current_run_queue,
    placement::{select_run_queue, task_run_queue},
};
use crate::{KtaskRef, TaskInner, TaskState, api::current};

/// The task routine for migrating the current task to the correct CPU.
///
/// It calls `select_run_queue` to get the correct run queue for the task, and
/// then enqueues the task through the ownership-aware entry.
pub(crate) fn migrate_entry(migrated_task: KtaskRef) {
    let rq = select_run_queue::<kspin::NoPreemptIrqSave>(&migrated_task);
    let now_ns = khal::time::monotonic_time_nanos();
    rq.inner.flush_running_runtime(now_ns);
    let decision = rq.inner.enqueue_task(migrated_task);
    rq.inner.kick(decision);
}

pub(super) fn spawn_affinity_migration_task(migrated: KtaskRef) -> KtaskRef {
    const MIGRATION_TASK_STACK_SIZE: usize = 16384;
    TaskInner::new_internal(
        move || migrate_entry(migrated),
        "migration-task".into(),
        MIGRATION_TASK_STACK_SIZE,
    )
    .into_arc()
}

fn try_remove_ready_task(task: &KtaskRef) -> Option<KtaskRef> {
    let rq = task_run_queue::<kspin::NoPreemptIrqSave>(task);
    if !task.is_ready() {
        return None;
    }
    let now_ns = khal::time::monotonic_time_nanos();
    let removed = rq.inner.with_sched(|s| s.dequeue(task, now_ns))?;
    Some(removed)
}

/// After [`KtaskRef::set_cpumask`], move the task off any CPU it currently
/// occupies that the new mask forbids.
///
/// Enforcement applies only while the task *occupies* a CPU: it is current,
/// running remotely, or queued on that CPU's runqueue. Prepared-but-not-
/// activated tasks, blocked/exited waiters, and the idle-pull steal-to-enqueue
/// gap are not occupying a CPU, so this is mask-only. The next enqueue
/// (`activate_task`, wake, or dest idle-pull enqueue) selects a CPU from the
/// new mask. Dest idle-pull still rechecks membership after enqueue.
///
/// - current: migrate immediately (same as historical `set_current_affinity`);
/// - ready on a forbidden RQ: dequeue and `migrate_entry`;
/// - not queued: mask-only;
/// - running remotely: request resched and wait for the affinity migrate in
///   [`CurrentRunQueueRef::preempt_resched`].
///
/// Returns `false` only when a remote running task cannot be migrated off a
/// newly forbidden CPU (no preempt/IPI path, or still there after the wait).
pub(crate) fn enforce_affinity_placement(task: &KtaskRef) -> bool {
    let cpumask = task.cpumask();

    if current().ptr_eq(task) {
        if !cpumask.get(this_cpu_id().as_usize()) {
            let migration_task = spawn_affinity_migration_task(task.clone());
            current_run_queue::<kspin::NoPreemptIrqSave>().migrate_current(migration_task);
            assert!(cpumask.get(this_cpu_id().as_usize()), "Migration failed");
        }
        return true;
    }

    if matches!(task.state(), TaskState::Blocked | TaskState::Exited) {
        return true;
    }

    if cpumask.get(task.cpu_id().as_usize()) {
        return true;
    }

    if let Some(removed) = try_remove_ready_task(task) {
        migrate_entry(removed);
        return !task.is_running() || cpumask.get(task.cpu_id().as_usize());
    }

    if task.is_running() {
        #[cfg(feature = "smp")]
        {
            let cpu_id = task.cpu_id();
            debug_assert_ne!(cpu_id, this_cpu_id());
            let resched_result = request_remote_resched(cpu_id);
            record_remote_resched(cpu_id, resched_result.is_err());
            // Same pattern as waiting on `on_cpu`: the remote CPU finishes the
            // affinity migrate at its next preempt safe point.
            while task.is_running() && !cpumask.get(task.cpu_id().as_usize()) {
                core::hint::spin_loop();
            }
        }
        #[cfg(not(feature = "smp"))]
        {
            return false;
        }
    }

    // Running→Ready on the old RQ can race the IPI wait; take one more dequeue.
    if !task.is_running()
        && !cpumask.get(task.cpu_id().as_usize())
        && let Some(removed) = try_remove_ready_task(task)
    {
        migrate_entry(removed);
    }

    // Occupying a forbidden CPU is failure. Anything off-queue is mask-only.
    !task.is_running() || cpumask.get(task.cpu_id().as_usize())
}

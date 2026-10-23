// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Affinity updates and cross-RQ migrate-in.

use khal::percpu::this_cpu_id;
use ksched::EnqueueKind;

#[cfg(feature = "smp")]
use super::rq::request_remote_resched;
#[cfg(feature = "smp")]
use super::stats::record_remote_resched;
use super::{current_run_queue, placement::select_run_queue, rq::with_owner_sched};
use crate::{KtaskRef, TaskInner, TaskState, api::current};

/// Migrate `migrated_task` onto a CPU its current cpumask allows.
///
/// `kind` is the enqueue the caller already chose, and every attempt uses it.
/// Replaying a rejected wake or first publish as [`EnqueueKind::MigrateIn`]
/// would skip Deadline's spent-budget throttle and a new task's replenish /
/// first placement. `publish_task` rejects a CPU that a concurrent
/// `set_cpumask` removed after `select_run_queue` sampled the mask; select
/// again outside that lock. No retry cap: giving up would leave the task on
/// no run queue.
///
/// Each attempt takes `NoPreemptIrqSave`. A caller that is already IRQ-off
/// (an outer run-queue guard, a hardirq, or `finish_task_switch`) stays
/// masked for the whole retry: the inner guard restores a still-disabled
/// state. A cpumask that keeps moving can hold that section open and delay
/// timers and IPIs until one publish succeeds. Publishing only after IRQs
/// are enabled is left for later.
pub(crate) fn migrate_entry(mut migrated_task: KtaskRef, kind: EnqueueKind) {
    loop {
        let rq = select_run_queue::<kspin::NoPreemptIrqSave>(&migrated_task);
        let now_ns = khal::time::monotonic_time_nanos();
        rq.inner.flush_running_runtime(now_ns);
        match rq.inner.publish_task(migrated_task, kind, now_ns) {
            Ok(decision) => {
                rq.inner.kick(decision);
                return;
            }
            Err(removed) => migrated_task = removed,
        }
    }
}

pub(super) fn spawn_affinity_migration_task(migrated: KtaskRef) -> KtaskRef {
    const MIGRATION_TASK_STACK_SIZE: usize = 16384;
    TaskInner::new_internal(
        move || migrate_entry(migrated, EnqueueKind::MigrateIn),
        "migration-task".into(),
        MIGRATION_TASK_STACK_SIZE,
    )
    .into_arc()
}

/// A ready task may be dequeued onto another CPU.
///
/// Yield/preempt requeue before `switch_to` finishes. `Ready` does not mean
/// the outgoing CPU has left the stack, and idle is never queued.
pub(super) fn can_dequeue_ready_task(task: &crate::KTask) -> bool {
    task.is_ready() && !task.on_cpu() && !task.is_idle()
}

/// Dequeues `task` once it is off-CPU.
///
/// `None` when `task` occupies no run queue. Yield/preempt requeue the runner
/// as `Ready` before `switch_to`, so a queued task may still be `on_cpu` while
/// the old CPU saves its context. Publishing it elsewhere then would resume
/// the same stack on two CPUs. Under the RQ lock a queued task cannot be
/// picked, so `!on_cpu` there is stable until the dequeue completes. Only that
/// `on_cpu` case spins, and the spin holds neither the RQ lock nor an IRQ
/// guard: `finish_task_switch` clears `on_cpu`. A miss while already off-CPU
/// is a detached seat (prepared but not activated, or the idle-pull
/// steal-to-enqueue gap). Retrying it would spin forever, since this caller
/// does not enqueue the task. Idle stays `Ready` + `on_cpu` for its whole run
/// and is never queued, so it must not enter the wait.
fn dequeue_ready_task_waiting_off_cpu(task: &KtaskRef) -> Option<KtaskRef> {
    if task.is_idle() {
        return None;
    }
    loop {
        if !task.is_ready() {
            return None;
        }
        let now_ns = khal::time::monotonic_time_nanos();
        let mut removed = None;
        let still_on_cpu = with_owner_sched(task, |s| {
            if !can_dequeue_ready_task(task.as_ref()) {
                return true;
            }
            removed = s.dequeue(task, now_ns);
            false
        });
        if !still_on_cpu {
            return removed;
        }
        while task.is_ready() && task.on_cpu() {
            core::hint::spin_loop();
        }
    }
}

/// After [`KtaskRef::set_cpumask`], move the task off any CPU it currently
/// occupies that the new mask forbids.
///
/// Enforcement applies only while the task *occupies* a CPU: it is current,
/// running remotely, or queued on that CPU's runqueue. Prepared-but-not-
/// activated tasks, blocked/exited waiters, and the idle-pull steal-to-enqueue
/// gap are not occupying a CPU, so this is mask-only. The next publish
/// (`activate_task`, wake, or dest idle-pull enqueue) rejects a CPU outside
/// the new mask under that run queue's lock.
///
/// - current: migrate immediately (same as historical `set_current_affinity`);
/// - ready on a forbidden RQ: dequeue and `migrate_entry`;
/// - not queued: mask-only;
/// - running remotely: request resched and wait for the affinity migrate in
///   [`CurrentRunQueueRef::preempt_resched`].
///
/// Returns `false` only when a remote running task cannot be migrated off a
/// newly forbidden CPU (no preempt/IPI path, or still there after the wait).
///
/// The first occupancy check samples `state` and `cpu_id` before the owner
/// lock. A publish that already read the previous mask can still enqueue on
/// a CPU this mask forbids, and this call can return success for that
/// in-flight publish. The task stays there until it leaves that run queue or
/// a later call observes the updated `cpu_id`.
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

    if let Some(removed) = dequeue_ready_task_waiting_off_cpu(task) {
        migrate_entry(removed, EnqueueKind::MigrateIn);
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
        && let Some(removed) = dequeue_ready_task_waiting_off_cpu(task)
    {
        migrate_entry(removed, EnqueueKind::MigrateIn);
    }

    // Occupying a forbidden CPU is failure. Anything off-queue is mask-only.
    !task.is_running() || cpumask.get(task.cpu_id().as_usize())
}

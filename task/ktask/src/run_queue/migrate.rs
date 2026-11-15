// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Affinity updates and cross-RQ migrate-in.

use khal::percpu::this_cpu_id;
use ksched::EnqueueKind;

use super::{placement::select_run_queue, rq::with_owner_sched};
use crate::{KCpuMask, KtaskRef, TaskInner, TaskState, api::current};

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
    TaskInner::new_internal_with_task(
        |migrated| migrate_entry(migrated, EnqueueKind::MigrateIn),
        migrated,
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
/// - current, or running remotely: `stop_one_cpu` on the CPU it occupies.
///   The Stop callback dequeues a ready off-CPU task. A task that is already
///   blocked stays mask-only; its next wake publishes under the destination
///   run-queue lock. `preempt_resched` / `yield_current` still migrate if
///   they notice the mask first;
/// - ready on a forbidden RQ and already off-CPU: dequeue and `migrate_entry`;
/// - not queued: mask-only.
///
/// Returns `false` when the stopper is not up (the previous mask is restored)
/// or the task is still running on a forbidden CPU after the stop.
///
/// The first occupancy check samples `state` and `cpu_id` before the owner
/// lock. A publish that already read the previous mask can still enqueue on
/// a CPU this mask forbids, and this call can return success for that
/// in-flight publish. The task stays there until it leaves that run queue or
/// a later call observes the updated `cpu_id`.
pub(crate) fn enforce_affinity_placement(task: &KtaskRef, old_mask: KCpuMask) -> bool {
    let cpumask = task.cpumask();

    if current().ptr_eq(task) {
        if !cpumask.get(this_cpu_id().as_usize()) {
            return stop_and_recheck(task, cpumask, old_mask);
        }
        return true;
    }

    let state = task.state();
    if state.is_blocked() || state == TaskState::Exited {
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
        return stop_and_recheck(task, cpumask, old_mask);
    }

    // A task can become Ready on the old RQ between the first dequeue and this
    // check. Take one more dequeue when it is off-CPU.
    if !task.is_running()
        && !cpumask.get(task.cpu_id().as_usize())
        && let Some(removed) = dequeue_ready_task_waiting_off_cpu(task)
    {
        migrate_entry(removed, EnqueueKind::MigrateIn);
    }

    // Occupying a forbidden CPU is failure. Anything off-queue is mask-only.
    !task.is_running() || cpumask.get(task.cpu_id().as_usize())
}

fn stop_and_recheck(task: &KtaskRef, mask: KCpuMask, old_mask: KCpuMask) -> bool {
    crate::api::assert_sleepable_context("set_task_affinity");
    let cpu = task.cpu_id();
    let pinned = task.clone();
    if !crate::stop_one_cpu(
        cpu,
        migration_cpu_stop,
        core::ptr::from_ref(&pinned).cast::<()>().cast_mut(),
    ) {
        // The stopper never ran, so the new mask would leave this task on a
        // CPU it now forbids.
        task.restore_effective_cpus(old_mask);
        return false;
    }
    if !task.is_running()
        && !mask.get(task.cpu_id().as_usize())
        && let Some(removed) = dequeue_ready_task_waiting_off_cpu(task)
    {
        migrate_entry(removed, EnqueueKind::MigrateIn);
    }
    !task.is_running() || mask.get(task.cpu_id().as_usize())
}

/// Stop-thread callback: dequeue a ready off-CPU `task` and migrate it.
///
/// A blocked task is not queued. Its wake selects a CPU the new mask allows
/// and `publish_task` moves `cpu_id` / `nr_home` under that run queue's
/// scheduler lock. Writing those fields here would race that publish: the
/// waker does not hold this CPU's run-queue lock. This runs with IRQs
/// disabled and must not spin on `on_cpu`.
fn migration_cpu_stop(arg: *mut ()) {
    // SAFETY: `stop_one_cpu` keeps the caller's `KtaskRef` on the stack until
    // this callback returns. `wait` does not drop that frame until `notify_done`.
    let task = unsafe { &*arg.cast::<KtaskRef>() };
    if task.is_idle() || task.cpu_id() != this_cpu_id() {
        return;
    }
    if task.state().is_blocked()
        || task.state() == TaskState::Exited
        || task.on_cpu()
        || !task.is_ready()
    {
        return;
    }
    let now_ns = khal::time::monotonic_time_nanos();
    let mut removed = None;
    with_owner_sched(task, |sched| {
        if can_dequeue_ready_task(task.as_ref()) {
            removed = sched.dequeue(task, now_ns);
        }
    });
    if let Some(removed) = removed {
        migrate_entry(removed, EnqueueKind::MigrateIn);
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Task APIs for multi-task configuration.

use alloc::{
    string::String,
    sync::{Arc, Weak},
};
use core::sync::atomic::AtomicUsize;

use kspin::NoPreemptIrqSave;

use crate::run_queue::task_run_queue;
pub(crate) use crate::run_queue::{current_run_queue, select_run_queue, select_wake_run_queue};
pub use crate::{
    task::{CurrentTask, KTaskExt, TaskExt, TaskId, TaskInner, TaskState},
    timers::register_timer_callback,
    wait_queue::WaitQueue,
};

/// The reference type of a task.
pub type KtaskRef = Arc<KTask>;

/// The weak reference type of a task.
pub type WeakKtaskRef = Weak<KTask>;

/// The wrapper type for [`KCpuMask`](kcpu_id_map::KCpuMask) with SMP configuration.
pub use kcpu_id_map::KCpuMask;

static CPU_NUM: AtomicUsize = AtomicUsize::new(1);

cfg_select! {
    feature = "sched_rr" => {
        const MAX_TIME_SLICE: usize = 5;
        pub(crate) type KTask = ksched::RRTask<TaskInner, MAX_TIME_SLICE>;
        pub(crate) type Scheduler = ksched::RRScheduler<TaskInner, MAX_TIME_SLICE>;
    }
    feature = "sched_cfs" => {
        pub(crate) type KTask = axsched::CFSTask<TaskInner>;
        pub(crate) type Scheduler = axsched::CFScheduler<TaskInner>;
    }
    feature = "sched_eevdf" => {
        const MAX_TIME_SLICE: usize = 5;
        pub(crate) type KTask = ksched::EevdfEntity<TaskInner, MAX_TIME_SLICE>;
        pub(crate) type Scheduler = ksched::EevdfScheduler<TaskInner, MAX_TIME_SLICE>;
    }
    _ => {
        // If no scheduler features are set, use FIFO as the default.
        pub(crate) type KTask = ksched::FifoTask<TaskInner>;
        pub(crate) type Scheduler = ksched::FifoScheduler<TaskInner>;
    }
}

#[cfg(feature = "preempt")]
struct KernelGuardIfImpl;

#[cfg(feature = "preempt")]
#[crate_interface::impl_interface]
impl kspin::KernelGuardIf for KernelGuardIfImpl {
    fn disable_preempt() {
        if let Some(curr) = current_may_uninit() {
            curr.disable_preempt();
        }
    }

    fn enable_preempt() {
        if let Some(curr) = current_may_uninit() {
            curr.enable_preempt(true);
        }
    }
}

/// Gets the current task, or returns [`None`] if the current task is not
/// initialized.
pub fn current_may_uninit() -> Option<CurrentTask> {
    CurrentTask::try_get()
}

/// Gets the current task.
///
/// # Panics
///
/// Panics if the current task is not initialized.
pub fn current() -> CurrentTask {
    CurrentTask::get()
}

/// Initializes the task scheduler (for the primary CPU).
pub fn init_scheduler() {
    init_scheduler_with_cpu_num(kbuild_config::CPU_NUM);
}

/// Initializes the task scheduler with cpu_num (for the primary CPU).
fn init_scheduler_with_cpu_num(cpu_num: usize) {
    info!("Initialize scheduling...");
    CPU_NUM.store(cpu_num, core::sync::atomic::Ordering::Relaxed);

    crate::run_queue::init();

    info!("  use {} scheduler.", Scheduler::scheduler_name());
}

pub(crate) fn active_cpu_num() -> usize {
    CPU_NUM.load(core::sync::atomic::Ordering::Relaxed)
}

/// Initializes the task scheduler for secondary CPUs.
pub fn init_scheduler_secondary() {
    crate::run_queue::init_secondary();
}

/// Handles periodic timer ticks for the task manager.
///
/// For example, advance scheduler states, checks timed events, etc.
pub fn on_timer_tick() {
    use kspin::NoOp;
    crate::timers::check_events();
    // Since irq and preemption are both disabled here,
    // we can get current run queue with the default `kspin::NoOp`.
    current_run_queue::<NoOp>().scheduler_timer_tick();
}

/// Adds the given task to the run queue, returns the task reference.
pub fn spawn_task(task: TaskInner) -> KtaskRef {
    let task_ref = task.into_arc();
    select_run_queue::<NoPreemptIrqSave>(&task_ref).add_task(task_ref.clone());
    task_ref
}

/// Spawns a new task with the given parameters.
///
/// Returns the task reference.
pub fn spawn_raw<F>(f: F, name: String, stack_size: usize) -> KtaskRef
where
    F: FnOnce() + Send + 'static,
{
    spawn_task(TaskInner::new(f, name, stack_size))
}

/// Spawns a new task with the given name and the default stack size ([`kbuild_config::TASK_STACK_SIZE`]).
///
/// Returns the task reference.
pub fn spawn_with_name<F>(f: F, name: String) -> KtaskRef
where
    F: FnOnce() + Send + 'static,
{
    spawn_raw(f, name, kbuild_config::TASK_STACK_SIZE)
}

/// Spawns a new task with the default parameters.
///
/// The default task name is an empty string. The default task stack size is
/// [`kbuild_config::TASK_STACK_SIZE`].
///
/// Returns the task reference.
pub fn spawn<F>(f: F) -> KtaskRef
where
    F: FnOnce() + Send + 'static,
{
    spawn_with_name(f, String::new())
}

/// Set the priority for current task.
///
/// The range of the priority is dependent on the underlying scheduler. For
/// example, in the [CFS] scheduler, the priority is the nice value, ranging from
/// -20 to 19.
///
/// Returns `true` if the priority is set successfully.
///
/// [CFS]: https://en.wikipedia.org/wiki/Completely_Fair_Scheduler
pub fn set_prio(prio: isize) -> bool {
    current_run_queue::<NoPreemptIrqSave>().set_current_priority(prio)
}

/// Set the priority for the specified task.
///
/// The interpretation of `prio` depends on the configured scheduler. For
/// fair schedulers, it matches the Linux nice range `-20..=19`.
pub fn set_task_prio(task: &KtaskRef, prio: isize) -> bool {
    task_run_queue::<NoPreemptIrqSave>(task).set_task_priority(task, prio)
}

/// Set the affinity for the current task.
/// [`KCpuMask`] is used to specify the CPU affinity.
/// Returns `true` if the affinity is set successfully.
///
/// TODO: support set the affinity for other tasks.
pub fn set_current_affinity(cpumask: KCpuMask) -> bool {
    if cpumask.is_empty() {
        false
    } else {
        let curr = current().clone();

        curr.set_cpumask(cpumask);
        // After setting the affinity, we need to check if current cpu matches
        // the affinity. If not, we need to migrate the task to the correct CPU.
        #[cfg(feature = "smp")]
        if !cpumask.get(khal::percpu::this_cpu_id().as_usize()) {
            const MIGRATION_TASK_STACK_SIZE: usize = 4096;
            // Spawn a new migration task for migrating.
            let migration_task = TaskInner::new(
                move || crate::run_queue::migrate_entry(curr),
                "migration-task".into(),
                MIGRATION_TASK_STACK_SIZE,
            )
            .into_arc();

            // Migrate the current task to the correct CPU using the migration task.
            current_run_queue::<NoPreemptIrqSave>().migrate_current(migration_task);

            assert!(
                cpumask.get(khal::percpu::this_cpu_id().as_usize()),
                "Migration failed"
            );
        }
        true
    }
}

/// Current task gives up the CPU time voluntarily, and switches to another
/// ready task.
pub fn yield_now() {
    current_run_queue::<NoPreemptIrqSave>().yield_current()
}

/// Current task is going to sleep for the given duration.
pub fn sleep(dur: core::time::Duration) {
    sleep_until(khal::time::monotonic_time() + dur);
}

/// Current task is going to sleep, it will be woken up at the given deadline.
pub fn sleep_until(deadline: khal::time::TimeValue) {
    crate::future::block_on(crate::future::sleep_until(deadline));
}

/// Exits the current task.
pub fn exit(exit_code: i32) -> ! {
    current_run_queue::<NoPreemptIrqSave>().exit_current(exit_code)
}

/// The idle task routine.
///
/// It runs an infinite loop that keeps calling [`yield_now()`].
pub fn run_idle() -> ! {
    loop {
        yield_now();
        #[cfg(feature = "arm-timer-resume-fixup")]
        let cntpct_before = khal::time::now_ticks();
        karch::await_interrupts();

        #[cfg(feature = "arm-timer-resume-fixup")]
        {
            let repaired = khal::time::handle_idle_return(cntpct_before);
            let cntpct_after = khal::time::now_ticks();
            if !repaired && cntpct_after < cntpct_before {
                warn!(
                    "[PM-DBG] idle WFI returned with counter regression! before={}, after={}, \
                     delta={}",
                    cntpct_before,
                    cntpct_after,
                    cntpct_before.wrapping_sub(cntpct_after)
                );
            }
        }
    }
}

/// Returns `true` when no suspicious long lock-waits are observed on this CPU.
/// Returns `false` when a task appears to have been waiting on a lock for too long.
///
/// Note: this is a *heuristic* watchdog check, not a full deadlock detector.
#[cfg(feature = "watchdog")]
pub fn check_mutex_deadlock(now: usize) -> bool {
    let mut ok = true;
    crate::task_registry::for_each_tracked_task(khal::percpu::this_cpu_id(), |weaktask| {
        if !ok {
            return;
        }
        if let Some(task) = weaktask.upgrade() {
            let Some((_lock, since)) = task.inner().waiting_snapshot() else {
                return;
            };

            let blocked = now.saturating_sub(since);
            if khal::time::t2ns(blocked as u64) > 20_000_000_000 {
                // suspect stall (20s)
                ok = false;
            }
        }
    });
    ok
}

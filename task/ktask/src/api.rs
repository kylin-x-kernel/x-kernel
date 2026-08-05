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
    task::{CurrentTask, TaskInner, TaskState, UserTaskRuntime},
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
#[kiface::provide]
impl kspin::KernelGuardIf {
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
    init_scheduler_with_cpu_num(kcpu_id_map::nr_cpus());
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

/// Duration of one periodic scheduler tick (`Kconfig` `TICKS_PER_SECOND`).
const PERIODIC_INTERVAL: ktime_types::TimeSpan = ktime_types::TimeSpan::from_nanos(
    ktime_types::NANOS_PER_SEC / kbuild_config::TICKS_PER_SECOND as u64,
);

/// Absolute monotonic deadline of the next periodic scheduler tick.
#[percpu::def_percpu]
static NEXT_TICK_DEADLINE: Option<ktime_types::MonotonicInstant> = None;

unsafe fn next_tick_deadline() -> Option<ktime_types::MonotonicInstant> {
    // SAFETY: the caller guarantees that preemption is disabled on this CPU.
    unsafe { *NEXT_TICK_DEADLINE.current_ref_raw() }
}

unsafe fn set_next_tick_deadline(deadline: ktime_types::MonotonicInstant) {
    // SAFETY: the caller guarantees that preemption is disabled on this CPU.
    unsafe { *NEXT_TICK_DEADLINE.current_ref_mut_raw() = Some(deadline) };
}

/// Re-arms the local CPU hardware timer to fire at the earlier of the next
/// periodic tick deadline and `earliest_soft_deadline` (if any).
///
/// # IRQ safety
/// The caller MUST hold the local timer-wheel `SpinNoIrq` lock (or otherwise
/// guarantee IRQs are masked on this CPU), so the timer IRQ handler cannot race
/// the periodic deadline read or the hardware register write.
pub(crate) fn rearm_local_timer(earliest_soft_deadline: Option<ktime_types::MonotonicInstant>) {
    // SAFETY: the caller guarantees IRQs are masked on this CPU (the wheel lock
    // is held or we run in IRQ context), so the timer IRQ cannot re-enter and
    // observe a half-updated slot. Only this CPU's own slot is touched.
    let next_tick =
        unsafe { next_tick_deadline() }.unwrap_or(ktime_types::MonotonicInstant::ORIGIN);
    let deadline = earliest_soft_deadline
        .map(|soft| soft.min(next_tick))
        .unwrap_or(next_tick);
    khal::time::arm_timer(deadline);
}

/// Hardware-timer IRQ entry. Drives the timer in a tick-bounded NOHZ style:
///
/// - Always drains the per-CPU soft-timer wheel and runs the wall-clock tick
///   callbacks (both are ns-driven, so safe to run on every hardware fire).
/// - Advances the scheduler tick **only** when the periodic tick deadline has
///   elapsed — the scheduler counts ticks rather than reading the clock, so it
///   must keep firing at `TICKS_PER_SECOND` even though the hardware may now
///   fire more often for sub-tick soft timers.
/// - Re-arms the hardware for the earlier of the next tick or the earliest
///   pending soft deadline, which is what makes sub-tick timeouts wake on time.
pub fn on_timer_fire() {
    use kspin::NoOp;
    let now = khal::time::monotonic_time();

    // Per-tick callbacks (ns-driven, safe to run on every hardware fire).
    crate::timers::check_events();

    // Drain expired timers and determine the next hardware deadline under a
    // single wheel-lock acquisition — avoids taking the lock twice (once for
    // drain via `check_timer_events`, once for rearm) on every timer IRQ.
    let cpu_id = khal::percpu::this_cpu_id();
    let (wakers, earliest) = crate::future::drain_expired_and_get_earliest(cpu_id);
    for (_, waker) in wakers {
        waker.wake();
    }

    // Advance the periodic scheduler tick. `cur == 0` is the first-fire lazy
    // init (avoids a catch-up loop while the slot still holds the sentinel).
    // SAFETY: the timer IRQ runs with IRQs (and preemption) disabled on this
    // CPU, so raw per-CPU access cannot race a migration to another CPU.
    // SAFETY: timer IRQ handling runs with preemption disabled on this CPU.
    let current_deadline = unsafe { next_tick_deadline() };
    if current_deadline.is_none_or(|deadline| now >= deadline) {
        // IRQ and preemption are both disabled here, so the default `NoOp`
        // spin guard suffices for obtaining the current run queue.
        current_run_queue::<NoOp>().scheduler_timer_tick();
        let next_deadline = now.checked_add(PERIODIC_INTERVAL).unwrap_or_else(|| {
            ktime_types::MonotonicInstant::from_span_since_origin(ktime_types::TimeSpan::MAX)
        });
        // SAFETY: as above.
        unsafe { set_next_tick_deadline(next_deadline) };
    }

    // Re-arm the hardware timer. `rearm_local_timer` requires IRQs off
    // (satisfied in the timer IRQ handler); the wheel lock is not needed here
    // since we already captured the earliest deadline above.
    crate::api::rearm_local_timer(earliest);
}

/// Consumes a pending preemption request before returning to ordinary task execution.
///
/// This is intended for task/user return paths that are about to resume normal
/// execution outside interrupt or exception handling. It is a no-op before the
/// current task is initialized, and it still obeys the normal preemption guards:
/// a context switch is only performed when the current task can be preempted and
/// the CPU is no longer inside an active exception context.
#[cfg(feature = "preempt")]
pub fn check_preempt_pending() {
    if current_may_uninit().is_some() {
        crate::task::TaskInner::current_check_preempt_pending();
    }
}

#[cfg(not(feature = "preempt"))]
pub fn check_preempt_pending() {}

/// Adds the given task to the run queue, returns the task reference.
pub fn spawn_task(task: TaskInner) -> KtaskRef {
    let task_ref = prepare_task(task);
    activate_task(&task_ref);
    task_ref
}

/// Converts a detached task into a shareable task reference without making it runnable.
///
/// This keeps the task in the initial `Ready` state but does not publish it to
/// any scheduler run queue yet. Callers that need additional publication steps
/// before the task may run should use this entry point together with
/// [`activate_task`].
pub fn prepare_task(task: TaskInner) -> KtaskRef {
    task.into_arc()
}

/// Publishes a prepared task to the scheduler run queue, making it runnable.
///
/// Callers must finish any required visibility or registry publication before
/// calling this function, because the task may start executing immediately
/// after it is enqueued.
pub fn activate_task(task: &KtaskRef) {
    select_run_queue::<NoPreemptIrqSave>(task).add_task(task.clone());
}

/// Spawns a PID-less kernel worker thread with the given parameters.
///
/// The new thread carries an `Internal` identity and therefore allocates no
/// root PID handle, mirroring FreeBSD's `kthread_add` (which attaches the
/// thread to the kernel's PID-0 process). This keeps the ordinary PID number
/// space reserved for user processes and explicitly visible kernel threads.
/// The thread is otherwise a normal runnable
/// kernel-context task: it gets time-sliced, may block, and exits normally.
///
/// Kernel threads that must own a Linux-visible PID should be built explicitly
/// with [`TaskInner::new_kthread`].
/// User threads must not use this helper; they must be created through the
/// process-domain staged publication path.
pub fn spawn_raw<F>(f: F, name: String, stack_size: usize) -> KtaskRef
where
    F: FnOnce() + Send + 'static,
{
    spawn_task(TaskInner::new_pidless_kthread(f, name, stack_size))
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
            let migration_task = TaskInner::new_internal(
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
pub fn sleep(dur: ktime_types::TimeSpan) {
    sleep_until(khal::time::monotonic_time() + dur);
}

/// Current task is going to sleep, it will be woken up at the given deadline.
pub fn sleep_until(deadline: ktime_types::MonotonicInstant) {
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
                    cntpct_before.as_raw(),
                    cntpct_after.as_raw(),
                    cntpct_before.wrapping_duration_since(cntpct_after).as_raw()
                );
            }
        }
    }
}

/// Dumps aggregate scheduler behavior counters when scheduler statistics are enabled.
pub fn dump_sched_stats() {
    crate::run_queue::dump_sched_stats();
}

/// Returns `true` when no suspicious long lock-waits are observed on this CPU.
/// Returns `false` when a task appears to have been waiting on a lock for too long.
///
/// Note: this is a *heuristic* watchdog check, not a full deadlock detector.
#[cfg(feature = "watchdog")]
pub fn check_mutex_deadlock(now: khal::time::TimerTicks) -> bool {
    let mut ok = true;
    crate::task_registry::for_each_tracked_task(khal::percpu::this_cpu_id(), |weaktask| {
        if !ok {
            return;
        }
        if let Some(task) = weaktask.upgrade() {
            let Some((_lock, since)) = task.inner().waiting_snapshot() else {
                return;
            };

            let blocked = now.wrapping_duration_since(since);
            if khal::time::ticks_to_span(blocked) > ktime_types::TimeSpan::from_secs(20) {
                // suspect stall (20s)
                ok = false;
            }
        }
    });
    ok
}

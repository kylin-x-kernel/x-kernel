// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Task APIs for multi-task configuration.

use alloc::{
    string::String,
    sync::{Arc, Weak},
};
use core::{future::poll_fn, sync::atomic::AtomicUsize, task::Poll};

use kspin::NoPreemptIrqSave;
#[cfg(feature = "execution-accounting")]
use ktime_types::MonotonicInstant;

use crate::run_queue::task_run_queue;
pub(crate) use crate::run_queue::{current_run_queue, select_run_queue, select_wake_run_queue};
pub use crate::{
    task::{CurrentTask, TaskInner, TaskState, UserTaskRuntime},
    timers::{register_timer_callback, register_timer_irq_note},
    wait_queue::WaitQueue,
};

/// The reference type of a task.
pub type KtaskRef = Arc<KTask>;

/// The weak reference type of a task.
pub type WeakKtaskRef = Weak<KTask>;

/// The wrapper type for [`KCpuMask`](kcpu_id_map::KCpuMask) with SMP configuration.
pub use kcpu_id_map::KCpuMask;

static CPU_NUM: AtomicUsize = AtomicUsize::new(1);

/// Opaque identity for an execution currently owned by a task.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskExecutionContext {
    words: [usize; 4],
}

impl TaskExecutionContext {
    /// Creates an opaque task-local execution identity.
    pub const fn new(words: [usize; 4]) -> Self {
        Self { words }
    }

    /// Returns all opaque context words.
    pub const fn words(self) -> [usize; 4] {
        self.words
    }

    /// Returns one opaque context word.
    pub const fn word(self, index: usize) -> Option<usize> {
        if index < self.words.len() {
            Some(self.words[index])
        } else {
            None
        }
    }
}

/// Scheduler bridge implemented by the crate that owns opaque executions.
#[cfg(feature = "execution-accounting")]
#[kiface::interface]
pub trait TaskExecutionAccountingIf {
    /// Accounts the current execution and returns the next tick deadline, if
    /// the execution still needs one.
    fn account_execution_tick(context: TaskExecutionContext) -> Option<MonotonicInstant>;

    /// Returns the current execution tick deadline without doing accounting.
    fn execution_tick_deadline(context: TaskExecutionContext) -> Option<MonotonicInstant>;
}

cfg_select! {
    feature = "sched_rr" => {
        const MAX_SLICE_NS: usize = ksched::DEFAULT_RR_SLICE_NS as usize;
        pub(crate) type KTask = ksched::RRTask<TaskInner, MAX_SLICE_NS>;
        pub(crate) type Scheduler = ksched::RRScheduler<TaskInner, MAX_SLICE_NS>;
    }
    feature = "sched_cfs" => {
        pub(crate) type KTask = axsched::CFSTask<TaskInner>;
        pub(crate) type Scheduler = axsched::CFScheduler<TaskInner>;
    }
    feature = "sched_eevdf" => {
        const MAX_SLICE_NS: usize = ksched::DEFAULT_SLICE_NS as usize;
        pub(crate) type KTask = ksched::EevdfEntity<TaskInner, MAX_SLICE_NS>;
        pub(crate) type Scheduler = ksched::EevdfScheduler<TaskInner, MAX_SLICE_NS>;
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

/// Returns the current monotonic time.
pub fn monotonic_time() -> ktime_types::MonotonicInstant {
    khal::time::monotonic_time()
}

/// Initializes the task scheduler for secondary CPUs.
pub fn init_scheduler_secondary() {
    crate::run_queue::init_secondary();
}

/// Absolute monotonic ns of the next schedule-evaluation deadline, or `0` if none.
#[percpu::def_percpu]
static NEXT_SCHED_DEADLINE_NS: u64 = 0;

/// Absolute monotonic ns of the current task execution-accounting tick.
#[percpu::def_percpu]
static NEXT_EXECUTION_TICK_DEADLINE_NS: u64 = 0;

/// When set, [`rearm_local_timer`] updates software slots only and skips the
/// hardware write. Used across timer-IRQ callback/drain/wake batches so
/// reentrant timer registration or wakeup accounting cannot program hardware
/// from an intermediate view before the final arbitration.
#[percpu::def_percpu]
static DEFER_LOCAL_TIMER_REARM: bool = false;

/// Minimum schedule-timer delay, matching Linux `hrtick_start()`.
///
/// Shorter delays do not buy fairness and can livelock the CPU in timer IRQs
/// when `next_preemption_ns` returns 0 (arm interval 0 → immediate re-entry
/// before `check_preempt_pending` runs). Soft/periodic deadlines are unaffected.
const MIN_SCHED_TIMER_NS: u64 = 10_000;

/// Picks the earliest absolute deadline among schedule / soft / periodic sources.
///
/// Returns [`None`] when every source is empty (hardware timer should disarm).
pub(crate) fn select_local_timer_deadline(
    sched: Option<u64>,
    worker: Option<u64>,
    soft: Option<u64>,
    periodic: Option<u64>,
) -> Option<u64> {
    [sched, worker, soft, periodic].into_iter().flatten().min()
}

/// Converts a scheduler-relative request into a one-shot absolute deadline and
/// an immediate-preemption flag.
fn schedule_deadline_request(now_ns: u64, rel_ns: Option<u64>) -> (Option<u64>, bool) {
    match rel_ns {
        None => (None, false),
        Some(0) => (None, true),
        Some(rel) => (
            Some(now_ns.saturating_add(rel.max(MIN_SCHED_TIMER_NS))),
            false,
        ),
    }
}

/// Re-arms the local CPU hardware timer to the earliest of schedule, execution,
/// soft, and periodic-callback deadlines. Disarms the hardware when none remain.
///
/// This is a pure hardware arbitration helper for scheduler deadlines: a due
/// scheduler slot is consumed only by timer IRQ / switch paths that have already
/// run scheduler accounting. This mirrors Linux hrtick start/restart behavior,
/// where reprogramming the hrtimer never substitutes for `task_tick()`.
///
/// When `periodic_earliest` is `Some`, it is used instead of rescanning the
/// per-CPU periodic-callback vector (e.g. after `run_due_and_earliest`).
///
/// # Synchronization
/// This function pins execution to the current CPU and masks local IRQs while
/// reading per-CPU deadline slots and programming hardware. Callers passing a
/// soft-timer deadline must still synchronize with that timer wheel while
/// computing `earliest_soft_deadline`.
pub(crate) fn rearm_local_timer(
    earliest_soft_deadline: Option<ktime_types::MonotonicInstant>,
    periodic_earliest: Option<u64>,
) {
    let _guard = NoPreemptIrqSave::new();
    // SAFETY: same guard as above.
    let sched_ns = unsafe { NEXT_SCHED_DEADLINE_NS.read_current_raw() };
    let sched = (sched_ns != 0).then_some(sched_ns);
    // SAFETY: same guard as above.
    let execution_ns = unsafe { NEXT_EXECUTION_TICK_DEADLINE_NS.read_current_raw() };
    let execution = (execution_ns != 0).then_some(execution_ns);
    // SAFETY: same guard as above; deferral is only toggled from timer IRQ.
    if unsafe { DEFER_LOCAL_TIMER_REARM.read_current_raw() } {
        return;
    }
    let soft = earliest_soft_deadline.map(|e| e.as_nanos_u64_saturating());
    let periodic = periodic_earliest.or_else(crate::timers::earliest_deadline);
    match select_local_timer_deadline(sched, execution, soft, periodic) {
        Some(d) => khal::time::arm_timer(ktime_types::MonotonicInstant::from_span_since_origin(
            ktime_types::TimeSpan::from_nanos(d),
        )),
        None => khal::time::disarm_timer(),
    }
}

/// Begins deferring hardware timer writes on this CPU (timer-IRQ wake batch).
pub(crate) fn begin_defer_local_timer_rearm() {
    // SAFETY: timer IRQ / IRQ-off on this CPU only.
    unsafe { DEFER_LOCAL_TIMER_REARM.write_current_raw(true) };
}

/// Ends deferred rearm mode. Caller must perform the final [`rearm_local_timer`].
pub(crate) fn end_defer_local_timer_rearm() {
    // SAFETY: timer IRQ / IRQ-off on this CPU only.
    unsafe { DEFER_LOCAL_TIMER_REARM.write_current_raw(false) };
}

/// Programs the next schedule-evaluation deadline from a relative `next_preemption_ns`.
///
/// Immediate requests (`Some(0)`) set `need_resched` on [`current`].
/// Prefer [`program_sched_deadline_for`] when the pending bit must land on an
/// incoming task during `switch_to`.
///
/// # IRQ safety
/// Caller must have IRQs (and preemption) disabled on this CPU.
pub(crate) fn program_sched_deadline(now_ns: u64, rel_ns: Option<u64>) {
    program_sched_deadline_for(None, now_ns, rel_ns);
}

/// Like [`program_sched_deadline`], but applies an immediate preemption request
/// to `task` when provided (e.g. the incoming task in `switch_to`).
pub(crate) fn program_sched_deadline_for(
    task: Option<&KtaskRef>,
    now_ns: u64,
    rel_ns: Option<u64>,
) {
    let (deadline, is_immediate) = schedule_deadline_request(now_ns, rel_ns);
    #[cfg(feature = "preempt")]
    if is_immediate {
        if let Some(task) = task {
            task.set_preempt_pending(true);
        } else if let Some(curr) = current_may_uninit() {
            curr.set_preempt_pending(true);
        }
    }
    #[cfg(not(feature = "preempt"))]
    {
        let _ = (is_immediate, task);
    }
    set_sched_deadline_ns(deadline);
}

/// Writes the absolute schedule-evaluation deadline slot (or clears it).
///
/// Prefer [`program_sched_deadline`] for scheduler-relative updates so the
/// Linux hrtick minimum and immediate-request semantics are applied.
///
/// # IRQ safety
/// Caller must have IRQs (and preemption) disabled on this CPU.
pub(crate) fn set_sched_deadline_ns(deadline_ns: Option<u64>) {
    // SAFETY: IRQ/preempt disabled; only this CPU's slot is written.
    unsafe {
        NEXT_SCHED_DEADLINE_NS.write_current_raw(deadline_ns.unwrap_or(0));
    }
}

fn set_execution_tick_deadline_ns(deadline_ns: Option<u64>) {
    // SAFETY: caller holds a preempt/IRQ guard or runs in scheduler/timer
    // context; only this CPU's slot is written.
    unsafe {
        NEXT_EXECUTION_TICK_DEADLINE_NS.write_current_raw(deadline_ns.unwrap_or(0));
    }
}

fn execution_tick_deadline_for_context(context: TaskExecutionContext) -> Option<u64> {
    #[cfg(feature = "execution-accounting")]
    {
        TaskExecutionAccountingIf::execution_tick_deadline(context)
            .map(|deadline| deadline.span_since_origin().as_nanos_u64_saturating())
    }
    #[cfg(not(feature = "execution-accounting"))]
    {
        let _ = context;
        None
    }
}

fn refresh_execution_tick_deadline_slot_for(context: Option<TaskExecutionContext>) {
    set_execution_tick_deadline_ns(context.and_then(execution_tick_deadline_for_context));
}

fn account_execution_tick_for_context(context: TaskExecutionContext) {
    #[cfg(feature = "execution-accounting")]
    let deadline = TaskExecutionAccountingIf::account_execution_tick(context)
        .map(|deadline| deadline.span_since_origin().as_nanos_u64_saturating());
    #[cfg(not(feature = "execution-accounting"))]
    let deadline = {
        let _ = context;
        None
    };
    set_execution_tick_deadline_ns(deadline);
}

pub(crate) fn account_execution_context(context: Option<TaskExecutionContext>) {
    match context {
        Some(context) => account_execution_tick_for_context(context),
        None => set_execution_tick_deadline_ns(None),
    }
}

pub(crate) fn refresh_execution_tick_deadline_for_task(task: &KtaskRef) {
    refresh_execution_tick_deadline_slot_for(task.execution_context());
}

/// Installs an opaque execution context on the current task.
pub fn set_current_execution_context(
    context: TaskExecutionContext,
) -> Option<TaskExecutionContext> {
    current().set_execution_context(context)
}

/// Clears the current task's opaque execution context if it matches.
pub fn clear_current_execution_context(context: TaskExecutionContext) -> bool {
    current().clear_execution_context(context)
}

/// Returns the current task's opaque execution context.
pub fn current_execution_context() -> Option<TaskExecutionContext> {
    current_may_uninit().and_then(|task| task.execution_context())
}

/// Refreshes the local execution-accounting tick deadline for current.
pub fn refresh_current_execution_tick() {
    let _guard = NoPreemptIrqSave::new();
    refresh_execution_tick_deadline_slot_for(current_execution_context());
    rearm_local_timer(
        crate::future::earliest_deadline(khal::percpu::this_cpu_id()),
        None,
    );
}

/// Hardware-timer IRQ entry for dynamic schedule + soft + periodic timers:
///
/// - Accounts real runtime for the current task first (so wake handling is not
///   charged to the wrong task). A due schedule slot runs Linux `entity_tick`
///   (`check_preempt_tick`); soft/periodic IRQs only account.
/// - Runs due periodic callbacks and drains the soft-timer wheel.
/// - Refreshes the schedule deadline (`next_preemption_ns` arms until-eligible
///   only for an ineligible WF_SYNC buddy) and re-arms hardware.
pub fn on_timer_fire() {
    use kspin::NoOp;
    let now_ns = khal::time::monotonic_time_nanos();
    let cpu_id = khal::percpu::this_cpu_id();
    // Any local timer IRQ means this CPU is still taking interrupts. Watchdog
    // hardlockup must not wait for the 4s sample callback: a busy lone task
    // (late-init / unittest yield loop) may not run that callback for >10s
    // even though schedule/soft/knet IRQs are arriving.
    crate::timers::run_timer_irq_note();

    let sched_due = {
        // SAFETY: timer IRQ context; IRQs are masked on this CPU, so only this
        // CPU's `NEXT_SCHED_DEADLINE_NS` slot is read.
        let sched_ns = unsafe { NEXT_SCHED_DEADLINE_NS.read_current_raw() };
        sched_ns != 0 && now_ns >= sched_ns
    };
    // Account the interrupted task before waking anyone else.
    // Linux `entity_tick` (`check_preempt_tick`) only runs on the schedule
    // slot, not on every soft/periodic IRQ.
    #[cfg(feature = "sched_eevdf")]
    if sched_due {
        current_run_queue::<NoOp>().account_sched_tick(now_ns);
    } else {
        current_run_queue::<NoOp>().account_current_runtime(now_ns);
    }
    #[cfg(not(feature = "sched_eevdf"))]
    current_run_queue::<NoOp>().account_current_runtime(now_ns);
    // Timer callbacks and soft-timer wakeups may register/cancel timers or
    // refresh scheduler slots. Defer their hardware writes until the final
    // schedule/soft/periodic arbitration below, after all wakeups have run.
    begin_defer_local_timer_rearm();
    let (periodic_earliest, periodic_due) = crate::timers::run_due_and_earliest(now_ns);
    #[cfg(not(feature = "sched_stat"))]
    let _ = periodic_due;

    let (wakers, _) = crate::future::drain_expired_and_get_earliest(cpu_id);

    #[cfg(feature = "sched_stat")]
    {
        let soft_due = !wakers.is_empty();
        crate::run_queue::note_timer_irq(sched_due, soft_due, periodic_due);
    }
    for (_, waker) in wakers {
        waker.wake();
    }
    end_defer_local_timer_rearm();

    #[cfg(not(feature = "sched_stat"))]
    let _ = sched_due;

    current_run_queue::<NoOp>().refresh_sched_deadline(now_ns);
    // Re-read after wakes so callbacks that armed a new soft timer are kept.
    let earliest_soft = crate::future::earliest_deadline(cpu_id);
    rearm_local_timer(earliest_soft, periodic_earliest);
}

/// Called from a remote reschedule IPI: request preemption.
///
/// Match the pre-NOHZ path: only set `need_resched`. Accounting and timer
/// refresh here raced the waker's `&mut RunQueue` and could reprogram (or
/// disarm) the target's schedule slot before the IRQ-tail `peer_preempts_curr`
/// probe. A failed probe arms the backup hrtick in `preempt_resched`.
#[cfg(all(feature = "preempt", feature = "smp"))]
pub(crate) fn on_remote_resched_ipi() {
    current().set_preempt_pending(true);
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

/// Wakes a blocked task and optionally requests rescheduling on its run queue.
///
/// If the task is already running, ready, or exited, this is a no-op. This is
/// intended for subsystems that keep a task reference but do not own the wait
/// queue the task may currently be blocked on (e.g. the VMM waking a vCPU
/// thread that is parked in an interruptible sleep).
pub fn wake_task(task: &KtaskRef, resched: bool) {
    select_wake_run_queue::<NoPreemptIrqSave>(task).unblock_task(task.clone(), resched);
}

/// Interrupts a task and wakes it if it is blocked.
///
/// Sets the task's interrupt flag so that an in-progress
/// [`interruptible`](crate::future::interruptible) wait returns early, then
/// unblocks it. If the task is already running, [`TaskInner::interrupt`] kicks
/// its CPU so a user-mode runner returns to the kernel. Used to deliver
/// asynchronous events (such as a pending virtual IRQ) to a thread parked in
/// [`interruptible_sleep_until`].
pub fn interrupt_task(task: &KtaskRef, resched: bool) {
    task.interrupt();
    wake_task(task, resched);
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

/// Set the CPU affinity mask for `task`.
///
/// Writes `cpumask` first. If `task` currently occupies a CPU the new mask
/// forbids (it is running there, or queued on that runqueue), it is migrated
/// off. Prepared tasks that have not been passed to [`activate_task`],
/// blocked/exited waiters, and the idle-pull steal-to-enqueue gap are not
/// occupying a CPU: only the mask changes, and the next enqueue selects a
/// legal CPU.
///
/// Returns `false` if `cpumask` is empty, or (on SMP) if a remote running task
/// could not be migrated off a newly forbidden CPU. On success the task is not
/// left running or queued on a CPU outside `cpumask`.
pub fn set_task_affinity(task: &KtaskRef, cpumask: KCpuMask) -> bool {
    if cpumask.is_empty() {
        return false;
    }
    task.set_cpumask(cpumask);
    #[cfg(feature = "smp")]
    {
        crate::run_queue::enforce_affinity_placement(task)
    }
    #[cfg(not(feature = "smp"))]
    {
        true
    }
}

/// Set the affinity for the current task.
///
/// Equivalent to [`set_task_affinity`] on [`current`].
pub fn set_current_affinity(cpumask: KCpuMask) -> bool {
    set_task_affinity(&current().clone(), cpumask)
}

/// Run `f` under a Linux-style `WF_SYNC` wake hint.
///
/// Wakes issued while `f` runs may sync-preempt an eligible next-buddy on the
/// wakee's run queue when the waker is expected to sleep soon (e.g. futex
/// wake followed by wait). Nested calls are refcounted.
pub fn with_wake_sync<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    let curr = current();
    curr.begin_wake_sync();
    let result = f();
    curr.end_wake_sync();
    result
}

/// Current task gives up the CPU time voluntarily, and switches to another
/// ready task.
pub fn yield_now() {
    current_run_queue::<NoPreemptIrqSave>().yield_current();
}

/// Blocks until `condition` becomes true, using `source` as the wake source.
///
/// The condition is checked before registration and again after registration so
/// callers do not lose a state change that races with waiter setup.
pub fn wait_for_poll_event_until<F>(
    event: &kpoll::PollEvent,
    mut condition: F,
) -> Result<(), kpoll::PollRegisterError>
where
    F: FnMut() -> bool,
{
    assert_sleepable_context("wait_for_poll_event_until");
    if condition() {
        return Ok(());
    }

    let mut registrations = kpoll::PollRegistrations::new();
    loop {
        let observed_generation = event.generation();
        crate::future::block_on(poll_fn(|cx| {
            if condition() {
                return Poll::Ready(Ok(true));
            }

            let mut context = registrations.context(cx);
            if let Err(error) = event.register(&mut context) {
                return Poll::Ready(Err(error));
            }
            drop(context);

            if condition() || event.has_changed_since(observed_generation) {
                Poll::Ready(Ok(true))
            } else {
                Poll::Pending
            }
        }))?;
        if condition() {
            return Ok(());
        }
    }
}

/// Current task is going to sleep for the given duration.
///
/// # Panics
///
/// Panics in hardirq, softirq, or BH-disabled context. Interrupt-like contexts
/// have no sleepable task state and must not enter the blocking scheduler path.
pub fn sleep(dur: ktime_types::TimeSpan) {
    assert_sleepable_context("sleep");
    sleep_until_unchecked(khal::time::monotonic_time() + dur);
}

/// Current task is going to sleep, it will be woken up at the given deadline.
///
/// # Panics
///
/// Panics in hardirq, softirq, or BH-disabled context. Interrupt-like contexts
/// have no sleepable task state and must not enter the blocking scheduler path.
pub fn sleep_until(deadline: ktime_types::MonotonicInstant) {
    assert_sleepable_context("sleep_until");
    sleep_until_unchecked(deadline);
}

/// Current task sleeps until `deadline`, or until another subsystem interrupts
/// it via [`interrupt_task`].
///
/// Unlike [`sleep_until`], the wait is wrapped in
/// [`interruptible`](crate::future::interruptible), so a concurrent
/// [`interrupt_task`] causes an early return. Used by the VMM WFI path so a
/// vCPU parked waiting for its next timer deadline can be woken immediately
/// when a virtual IRQ is injected.
///
/// # Panics
///
/// Panics in hardirq, softirq, or BH-disabled context. Interrupt-like contexts
/// have no sleepable task state and must not enter the blocking scheduler path.
pub fn interruptible_sleep_until(deadline: ktime_types::MonotonicInstant) {
    assert_sleepable_context("interruptible_sleep_until");
    if let Err(error) = crate::future::block_on(crate::future::interruptible(
        crate::future::sleep_until(deadline),
    )) && !error.is_signal()
    {
        log::error!(
            "interruptible_sleep_until failed to register interrupt wait: {}",
            error
        );
    }
}

fn assert_sleepable_context(operation: &'static str) {
    assert!(
        can_sleep_current_context(),
        "{operation} called from non-sleepable interrupt-like context"
    );
}

fn sleep_until_unchecked(deadline: ktime_types::MonotonicInstant) {
    crate::future::block_on(crate::future::sleep_until(deadline));
}

fn can_sleep_current_context() -> bool {
    !kirq::context::is_in_interrupt_context()
}

#[cfg(unittest)]
pub fn can_sleep_current_context_for_tests() -> bool {
    can_sleep_current_context()
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

/// Returns a text snapshot of per-CPU scheduler counters.
///
/// This is intended for process-context diagnostics such as
/// `/proc/sched_stat`; unlike [`dump_sched_stats`], it may allocate.
pub fn sched_stats_text() -> alloc::string::String {
    crate::run_queue::sched_stats_text()
}

#[cfg(unittest)]
mod tests_api {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicBool, Ordering};

    use kirq::{
        context::local_bh_disable,
        softirq::{
            SoftirqRunResult, SoftirqVec, raise_softirq,
            test_support::{ScopedSoftirqAction, begin_softirq_test},
        },
    };
    use unittest::{assert, assert_eq, def_test};

    use super::{
        MIN_SCHED_TIMER_NS, can_sleep_current_context_for_tests, current, sched_stats_text,
        schedule_deadline_request, select_local_timer_deadline, wait_for_poll_event_until,
        with_wake_sync,
    };

    static SOFTIRQ_SLEEPABLE_OBSERVED: AtomicBool = AtomicBool::new(true);

    fn record_sleep_gate_from_softirq() {
        SOFTIRQ_SLEEPABLE_OBSERVED.store(can_sleep_current_context_for_tests(), Ordering::Release);
    }

    #[def_test]
    fn timer_arbiter_picks_earliest_deadline() {
        assert_eq!(
            select_local_timer_deadline(Some(30), Some(15), Some(10), Some(20)),
            Some(10)
        );
        assert_eq!(
            select_local_timer_deadline(Some(50), Some(5), Some(8), None),
            Some(5)
        );
        assert_eq!(
            select_local_timer_deadline(None, None, Some(7), None),
            Some(7)
        );
    }

    #[def_test]
    fn timer_arbiter_disarms_when_all_empty() {
        assert_eq!(select_local_timer_deadline(None, None, None, None), None);
    }

    #[def_test]
    fn timer_arbiter_keeps_sched_when_soft_fires_early() {
        // Soft IRQ at T=10 must not clear a later schedule deadline at T=50;
        // re-arbitration after soft drain still sees the schedule source.
        let after_soft = select_local_timer_deadline(Some(50), None, None, None);
        assert_eq!(after_soft, Some(50));
    }

    #[def_test]
    fn timer_arbiter_past_deadline_still_selected() {
        // Expired deadlines remain the earliest absolute value; the IRQ path
        // re-accounts and reprograms rather than skipping them.
        assert_eq!(
            select_local_timer_deadline(Some(1), Some(100), Some(150), Some(200)),
            Some(1)
        );
    }

    #[def_test]
    fn immediate_schedule_request_sets_pending_without_rearming_hrtick() {
        assert_eq!(schedule_deadline_request(100, Some(0)), (None, true));
        assert_eq!(
            schedule_deadline_request(100, Some(1)),
            (Some(100 + MIN_SCHED_TIMER_NS), false)
        );
        assert_eq!(schedule_deadline_request(100, None), (None, false));
    }

    #[def_test(serial)]
    fn with_wake_sync_nested_refcount() {
        // `unittest::assert!` returns from the enclosing fn, so capture flags
        // inside `with_wake_sync` closures instead of asserting there.
        let mut outer_active = false;
        let mut inner_active = false;
        let mut after_inner_still_active = false;

        assert!(!current().is_wake_sync());
        with_wake_sync(|| {
            outer_active = current().is_wake_sync();
            with_wake_sync(|| {
                inner_active = current().is_wake_sync();
            });
            after_inner_still_active = current().is_wake_sync();
        });

        assert!(outer_active);
        assert!(inner_active);
        assert!(after_inner_still_active);
        assert!(!current().is_wake_sync());
    }

    #[def_test(serial)]
    fn wait_for_poll_event_until_blocks_until_condition_and_notify() {
        let event = Arc::new(kpoll::PollEvent::new());
        let ready = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let waiter_event = event.clone();
        let waiter_ready = ready.clone();
        let waiter_finished = finished.clone();

        crate::spawn(move || {
            wait_for_poll_event_until(&waiter_event, || waiter_ready.load(Ordering::Acquire))
                .expect("poll wait should register from task context");
            waiter_finished.store(true, Ordering::Release);
        });

        for _ in 0..8 {
            crate::yield_now();
        }
        assert!(!finished.load(Ordering::Acquire));

        ready.store(true, Ordering::Release);
        event.notify();
        for _ in 0..1000 {
            if finished.load(Ordering::Acquire) {
                break;
            }
            crate::yield_now();
        }
        assert!(finished.load(Ordering::Acquire));
    }

    #[def_test]
    fn sched_stats_text_matches_feature_gates() {
        let text = sched_stats_text();
        #[cfg(feature = "sched_stat")]
        {
            assert!(text.contains("[sched_stat] begin"));
            assert!(text.contains("wakeup_last_cpu="));
            assert!(text.contains("wakeup_idle_sibling="));
            assert!(text.contains("wakeup_fallback="));
            assert!(text.contains("idle_pull="));
            assert!(text.contains("timer_irq_sched="));
            assert!(text.contains("timer_irq_stale="));
            assert!(text.contains("[sched_stat] end"));
            #[cfg(all(feature = "sched_eevdf", feature = "smp"))]
            assert!(text.contains("[eevdf_stat]"));
            #[cfg(not(all(feature = "sched_eevdf", feature = "smp")))]
            assert!(!text.contains("[eevdf_stat]"));
        }
        #[cfg(not(feature = "sched_stat"))]
        {
            assert!(text.contains("disabled"));
        }
    }

    #[def_test(serial)]
    fn sleep_context_gate_rejects_softirq_context() {
        let _wake_gate = begin_softirq_test();
        SOFTIRQ_SLEEPABLE_OBSERVED.store(true, Ordering::Release);
        let _softirq_action =
            ScopedSoftirqAction::install(SoftirqVec::Block, record_sleep_gate_from_softirq);

        raise_softirq(SoftirqVec::Block);
        assert_eq!(kirq::softirq::run_pending_softirqs(), SoftirqRunResult::Ran);

        assert!(!SOFTIRQ_SLEEPABLE_OBSERVED.load(Ordering::Acquire));
    }

    #[def_test(serial)]
    fn sleep_context_gate_rejects_bh_disabled_context() {
        let _bh = local_bh_disable();
        assert!(!can_sleep_current_context_for_tests());
    }
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

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Task APIs for multi-task configuration.

use alloc::{
    string::String,
    sync::{Arc, Weak},
};
use core::{future::poll_fn, sync::atomic::AtomicUsize, task::Poll};

use kcpu_slot::cpu_slot_cell;
use kspin::NoPreemptIrqSave;

pub(crate) use crate::run_queue::{current_run_queue, select_run_queue, select_wake_run_queue};
pub use crate::{
    task::{CurrentTask, TaskInner, TaskState, UserTaskRuntime},
    timers::register_timer_irq_note,
    wait_queue::WaitQueue,
};

/// The reference type of a task.
pub type KtaskRef = Arc<KTask>;

/// The weak reference type of a task.
pub type WeakKtaskRef = Weak<KTask>;

/// The wrapper type for [`KCpuMask`](kcpu_id_map::KCpuMask) with SMP configuration.
pub use kcpu_id_map::KCpuMask;

static CPU_NUM: AtomicUsize = AtomicUsize::new(1);

// Capture the permanent root before user tasks can replace the active root.
// TaskContext::new_kernel chooses the architecture's kernel-only context state.
pub(crate) static KERNEL_PAGE_TABLE_ROOT: lazyinit::LazyInit<karch::HwPageTableRoot> =
    lazyinit::LazyInit::new();

pub(crate) type KTask = ksched::SchedTask<TaskInner>;
pub(crate) type Scheduler = ksched::Scheduler<TaskInner>;

#[kiface::provide]
impl kspin::KernelGuardIf {
    fn enable_preempt_without_check() {
        if let Some(curr) = current_may_uninit() {
            curr.enable_preempt(false);
        }
    }

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

#[kiface::provide]
impl khal::irq::IrqExitIf {
    fn check_preempt_pending() {
        if current_may_uninit().is_some() {
            TaskInner::current_check_preempt_pending_at_irq_exit();
        }
    }
}

/// Refreshes and returns the current task's execution time in nanoseconds.
///
/// Uses the scheduler's accounting clock, independent of user/system time and
/// scheduling weight. Takes the local RQ lock, advances scheduler accounting,
/// and may request preemption of the current task. IRQs and preemption are
/// disabled across task selection, refresh and read. Call after scheduler
/// initialization, without an RQ lock.
/// For another task's committed value, use `KtaskRef::sum_exec_runtime_ns()`.
///
/// # Panics
///
/// Panics if the current task or its run queue has not been initialized.
pub fn refresh_current_execution_runtime_ns() -> u64 {
    let mut rq = current_run_queue::<NoPreemptIrqSave>();
    rq.account_current_runtime(khal::time::monotonic_time_nanos());
    current().sum_exec_runtime_ns()
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

/// Returns whether the current task's actual preemption-disable count is nonzero.
///
/// This does not infer exclusion from IRQ state or scheduler feature flags.
/// Requires an initialized current task.
pub fn current_preemption_disabled() -> bool {
    current().preempt_section_held()
}

/// Marks the current task for rescheduling without waking it or scheduling.
///
/// Requires an initialized current task. A later normal scheduling checkpoint
/// must consume the request; this function sends no progress notification.
pub fn request_current_reschedule() {
    current().set_preempt_pending(true);
}

/// Runs an operation with preemption disabled, deferring the exit-time check.
///
/// Removes only this helper's preemption credit on return or unwind, without
/// checking pending work. The helper does not change IRQ state. The operation
/// must not voluntarily block or consume this helper's preemption credit.
/// The caller must arrange a later normal preemption checkpoint, including on
/// unwind. This helper is not available in NMI context.
///
/// # Panics
/// Panics in NMI context or if the current task is not initialized.
pub fn with_preempt_check_deferred<R>(operation_fn: impl FnOnce() -> R) -> R {
    struct DeferredPreemptCheck(CurrentTask);

    impl Drop for DeferredPreemptCheck {
        fn drop(&mut self) {
            self.0.enable_preempt(false);
        }
    }

    assert!(!kirq::context::is_in_nmi());
    let task = current();
    task.disable_preempt();
    let _deferred_check = DeferredPreemptCheck(task);
    operation_fn()
}

#[cfg(unittest)]
unittest::path_mod_test!(/// Current-task execution controls for deterministic unit-test fixtures.
pub mod test_support, "../ut-tests/api/test_support.rs");

/// Updates the current user task's saved root and activates it on this CPU.
///
/// # Panics
/// Panics if there is no current task; debug builds also check exclusion/user state.
///
/// # Safety
/// IRQs and preemption must already be disabled. The caller must publish a
/// matching current process binding and retain both old and new roots through
/// activation. The new root must preserve kernel mappings, and on AArch64 its
/// ASID must be prepared on this CPU. No borrowed task context may span this
/// call, and user execution must wait until its image/context are ready.
pub unsafe fn replace_current_user_page_table(root: karch::HwPageTableRoot) {
    debug_assert!(!karch::local_irq_enabled());
    let task = current();
    debug_assert!(task.user_runtime().is_some());
    #[cfg(feature = "preempt")]
    debug_assert!(task.preempt_section_held());
    // SAFETY: this is the running task, exclusively owned by this CPU for
    // context mutation with IRQs/preemption excluded. The caller forbids
    // outstanding context borrows and retains the valid kernel-mapped root.
    unsafe { (*task.ctx_mut_ptr()).set_page_table_root(root) };
    // SAFETY: the published binding retains this root and kernel mappings;
    // the caller prepared its CPU-local ASID and excludes context switches.
    unsafe { karch::write_user_page_table(root) };
    // CR3 writes flush on x86; AArch64 uses prepared ASIDs and TTBR0's ISB.
    // Untagged RISC-V/LoongArch roots need the switcher's explicit local flush.
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    karch::flush_tlb(None);
    #[cfg(unittest)]
    {
        // Check before a switch-in can repair a stale saved root on AArch64.
        assert_eq!(current_user_page_table_root(), root);
        assert_eq!(karch::read_user_page_table(), root);
    }
}

/// Reads the current task's saved page-table root for unit-test observations.
///
/// Excludes local IRQs and preemption while reading; does not prepare an ASID
/// or change the hardware root. Callers comparing hardware must keep their own
/// exclusion guard across both reads. Does not sleep or retain a task reference.
///
/// # Panics
/// Panics if the scheduler has no current task.
#[cfg(unittest)]
pub fn current_user_page_table_root() -> karch::HwPageTableRoot {
    let _pin = NoPreemptIrqSave::new();
    let task = current();
    #[cfg(target_arch = "aarch64")]
    let root = task.ctx().ttbr0_el1;
    #[cfg(target_arch = "x86_64")]
    let root = task.ctx().cr3;
    #[cfg(any(target_arch = "riscv32", target_arch = "riscv64"))]
    let root = task.ctx().satp;
    #[cfg(target_arch = "loongarch64")]
    let root = task.ctx().pgdl;
    root
}

/// Initializes the task scheduler (for the primary CPU).
///
/// The permanent kernel page table must already be active. Its root is retained
/// for kernel-context construction on every CPU. User-task callers must install
/// their own address-space root before making a task runnable.
/// Call this once, before secondary schedulers or other tasks are created.
pub fn init_scheduler() {
    init_scheduler_with_cpu_num(kcpu_id_map::nr_cpus());
}

/// Initializes the task scheduler with cpu_num (for the primary CPU).
fn init_scheduler_with_cpu_num(cpu_num: usize) {
    info!("Initialize scheduling...");
    KERNEL_PAGE_TABLE_ROOT.init_once(karch::read_kernel_page_table());
    CPU_NUM.store(cpu_num, core::sync::atomic::Ordering::Relaxed);

    crate::run_queue::init();

    info!("  use Fair/Idle class arbitrator.");
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

cpu_slot_cell! {
    /// Absolute monotonic ns of the next schedule-evaluation deadline, or
    /// `0` if none.
    static NEXT_SCHED_DEADLINE_NS: u64 = 0;
}

cpu_slot_cell! {
    /// When set, [`rearm_local_timer`] updates software slots only and skips
    /// the hardware write. Used across timer-IRQ callback/drain/wake batches
    /// so reentrant timer registration or wakeup accounting cannot program
    /// hardware from an intermediate view before the final arbitration.
    static DEFER_LOCAL_TIMER_REARM: bool = false;
}

/// Minimum schedule-timer delay, matching Linux `hrtick_start()`.
///
/// Shorter delays do not buy fairness and can livelock the CPU in timer IRQs
/// when `next_preemption_ns` returns 0 (arm interval 0 → immediate re-entry
/// before `check_preempt_pending` runs). Owned timer deadlines are unaffected.
const MIN_SCHED_TIMER_NS: u64 = 10_000;

/// Picks the earliest absolute deadline across all local timer sources.
///
/// Returns [`None`] when every source is empty (hardware timer should disarm).
pub(crate) fn select_local_timer_deadline(
    sched: Option<u64>,
    prepared: Option<u64>,
) -> Option<u64> {
    [sched, prepared].into_iter().flatten().min()
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

/// Re-arms the local CPU hardware timer to the earliest schedule or prepared
/// deadline. Disarms when neither source remains.
///
/// This is a pure hardware arbitration helper for scheduler deadlines: a due
/// scheduler slot is consumed only by timer IRQ / switch paths that have already
/// run scheduler accounting. This mirrors Linux hrtick start/restart behavior,
/// where reprogramming the hrtimer never substitutes for `task_tick()`.
///
/// # Synchronization
/// This function pins execution to the current CPU and masks local IRQs while
/// reading per-CPU deadline slots and programming hardware. The prepared CPU
/// lock is held through the hardware write so remote starts cannot be lost.
pub(crate) fn rearm_local_timer() {
    let guard = NoPreemptIrqSave::new();
    let sched_ns = NEXT_SCHED_DEADLINE_NS.read(&guard);
    let sched = (sched_ns != 0).then_some(sched_ns);
    if DEFER_LOCAL_TIMER_REARM.read(&guard) {
        return;
    }
    // SAFETY: the guard pins this CPU with IRQs masked. The static set and all
    // participating storage remain live. The programmer only selects values
    // and writes hardware, without acquiring timer/runqueue locks or callbacks.
    unsafe {
        crate::timers::PREPARED_TIMERS.reprogram_local_timer(
            khal::percpu::this_cpu_id(),
            |prepared| match select_local_timer_deadline(sched, prepared) {
                Some(d) => {
                    khal::time::arm_timer(ktime_types::MonotonicInstant::from_span_since_origin(
                        ktime_types::TimeSpan::from_nanos(d),
                    ))
                }
                None => khal::time::disarm_timer(),
            },
        )
    }
    .expect("current CPU must have a prepared timer slot");
}

/// Begins deferring hardware timer writes on this CPU (timer-IRQ wake batch).
pub(crate) fn begin_defer_local_timer_rearm() {
    DEFER_LOCAL_TIMER_REARM.store_current(true);
}

/// Ends deferred rearm mode. Caller must perform the final [`rearm_local_timer`].
pub(crate) fn end_defer_local_timer_rearm() {
    DEFER_LOCAL_TIMER_REARM.store_current(false);
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
    if is_immediate {
        if let Some(task) = task {
            task.set_preempt_pending(true);
        } else if let Some(curr) = current_may_uninit() {
            curr.set_preempt_pending(true);
        }
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
    NEXT_SCHED_DEADLINE_NS.store_current(deadline_ns.unwrap_or(0));
}

/// Hardware-timer IRQ entry for scheduling and owned timers:
///
/// - Accounts real runtime for the current task first (so wake handling is not
///   charged to the wrong task). A due schedule slot runs Linux `entity_tick`
///   (`check_preempt_tick`); other timer IRQs only account.
/// - Runs due owned timer callbacks, including async wait completion.
/// - Refreshes the schedule deadline (`next_preemption_ns` arms until-eligible
///   only for an ineligible WF_SYNC buddy) and re-arms hardware.
///
/// # Panics
///
/// Panics if the current logical CPU ID is outside `0..kbuild_config::NR_CPUS`:
/// prepared-timer dispatch or final hardware reprogramming rejects that CPU,
/// and this entry treats the missing timer slot as an initialization invariant
/// violation. Timer callbacks, IRQ instrumentation and wakers invoked here must
/// not panic; this entry does not catch their panics.
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
        let sched_ns = NEXT_SCHED_DEADLINE_NS.load_current();
        sched_ns != 0 && now_ns >= sched_ns
    };
    // Account the interrupted task before waking anyone else.
    // Linux `entity_tick` (`check_preempt_tick`) only runs on the schedule
    // slot, not on every owned timer IRQ.
    if sched_due {
        current_run_queue::<NoOp>().account_sched_tick(now_ns);
    } else {
        current_run_queue::<NoOp>().account_current_runtime(now_ns);
    }
    // Timer callbacks and their wakeups may register/cancel timers or
    // refresh scheduler slots. Defer their hardware writes until the final
    // arbitration below, after all wakeups have run.
    begin_defer_local_timer_rearm();

    // SAFETY: this is the matching CPU's non-nested timer IRQ, with IRQs masked.
    // The static set's registration boundary preserves node/handler storage.
    // No timer or runqueue lock is held; the platform clock is monotonic.
    let prepared_due = unsafe {
        crate::timers::PREPARED_TIMERS.dispatch_due(cpu_id, khal::time::monotonic_time_nanos)
    }
    .expect("current CPU must have a prepared timer slot");
    #[cfg(not(feature = "sched_stat"))]
    let _ = prepared_due;

    #[cfg(feature = "sched_stat")]
    {
        // Keep the diagnostic field layout; the removed periodic source is zero.
        crate::run_queue::note_timer_irq(sched_due, prepared_due, false);
    }
    end_defer_local_timer_rearm();

    #[cfg(not(feature = "sched_stat"))]
    let _ = sched_due;

    current_run_queue::<NoOp>().refresh_sched_deadline(now_ns);
    rearm_local_timer();
}

/// Called from a remote reschedule IPI: request preemption.
///
/// Only set `need_resched`. The IRQ-tail probe arms the hrtick.
#[cfg(feature = "smp")]
pub(crate) fn on_remote_resched_ipi() {
    current().set_preempt_pending(true);
}

#[cfg(feature = "ipi")]
#[kiface::provide]
impl kipi::RescheduleIpiIf {
    fn on_reschedule() {
        #[cfg(feature = "smp")]
        on_remote_resched_ipi();
    }
}

#[cfg(feature = "ipi")]
#[kiface::provide]
impl kipi::TimerReprogramIpiIf {
    fn on_timer_reprogram() {
        // IRQs stay masked through arbitration. No accounting, scheduling or
        // timer delivery belongs in this notification endpoint.
        rearm_local_timer();
    }
}

/// Consumes a pending preemption request before returning to ordinary task execution.
///
/// This is intended for task/user return paths that are about to resume normal
/// execution outside interrupt or exception handling. It is a no-op before the
/// current task is initialized, and it still obeys the normal preemption guards:
/// a context switch requires enabled local IRQs, a preemptible current task and
/// no active exception context. IRQ-masked callers leave the request pending;
/// the completed normal IRQ tail has a separate scheduler checkpoint.
pub fn check_preempt_pending() {
    if current_may_uninit().is_some() {
        crate::task::TaskInner::current_check_preempt_pending();
    }
}

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

/// Set the nice value of the current task.
///
/// Only `nice` changes. FIFO/RR policy and RT priority stay as they are.
/// The value is the Linux nice range `-20..=19`.
///
/// Returns `true` if the priority is set successfully.
pub fn set_prio(prio: isize) -> bool {
    current_run_queue::<NoPreemptIrqSave>().set_current_priority(prio)
}

/// Set the nice value of `task`.
///
/// Only `nice` changes. FIFO/RR policy and RT priority stay as they are.
/// The value is the Linux nice range `-20..=19`.
pub fn set_task_prio(task: &KtaskRef, prio: isize) -> bool {
    crate::run_queue::set_task_nice(task, prio)
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
/// wake followed by wait). Use it when the wakee consumes what the waker just
/// produced (futex wake, pipe data, a signal); open/close-style state changes
/// use ordinary wakes. On SMP, if this CPU's `nr_running`
/// is 1, the wakee stays on this CPU instead of an idle sibling unless a
/// placement veto applies (affinity today). A wake from hardirq or an active
/// softirq drops the hint for both placement and the enqueue mark: `current`
/// is the interrupted task. The IRQ-exit tail can still leak it onto the
/// pinned ksoftirqd wake. Nested calls are refcounted.
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
///
/// # Panics
///
/// Panics inside an explicit RCU read scope, which permits involuntary
/// preemption but not voluntary context switches.
pub fn yield_now() {
    assert!(
        !current().rcu_reader.is_reading(),
        "yield in RCU read scope"
    );
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
/// Panics in hardirq, softirq, or BH-disabled context, and in task context
/// with local IRQs disabled or a no-preempt section held: those contexts have
/// no stable CPU identity across the blocking switch and must not enter the
/// blocking scheduler path.
/// Explicit RCU read scopes also forbid voluntary sleeping.
pub fn sleep(dur: ktime_types::TimeSpan) {
    assert_sleepable_context("sleep");
    sleep_until_unchecked(khal::time::monotonic_time() + dur);
}

/// Current task is going to sleep, it will be woken up at the given deadline.
///
/// # Panics
///
/// Panics in hardirq, softirq, or BH-disabled context, and in task context
/// with local IRQs disabled or a no-preempt section held: those contexts have
/// no stable CPU identity across the blocking switch and must not enter the
/// blocking scheduler path.
/// Explicit RCU read scopes also forbid voluntary sleeping.
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
/// Panics in hardirq, softirq, or BH-disabled context, and in task context
/// with local IRQs disabled or a no-preempt section held: those contexts have
/// no stable CPU identity across the blocking switch and must not enter the
/// blocking scheduler path.
/// Explicit RCU read scopes also forbid voluntary sleeping.
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
        is_sleepable_context(),
        "{operation} called from non-sleepable context: no non-idle current task, interrupt-like, \
         local IRQs disabled, no-preempt section held, or active RCU read scope"
    );
}

fn sleep_until_unchecked(deadline: ktime_types::MonotonicInstant) {
    crate::future::block_on(crate::future::sleep_until(deadline));
}

fn can_sleep_task_context(task: &KtaskRef) -> bool {
    if task.is_idle() || kirq::context::is_in_interrupt_context() {
        return false;
    }
    // Blocking must not run with local IRQs masked: the task would switch out
    // inside an IRQ-off window and could resume on another CPU while the
    // masked section still assumes it is pinned to this one (see
    // `CpuPin` in `kcpu_slot`).
    if !karch::local_irq_enabled() {
        return false;
    }
    // A held no-preempt section must not span a switch either; its preempt
    // credit would leak across the switched-out interval. Explicit RCU scopes
    // have a separate no-sleep contract, without raising that preempt count.
    if task.preempt_section_held() || task.rcu_reader.is_reading() {
        return false;
    }
    true
}

/// Reports whether the current task can enter a blocking wait now.
///
/// Requires an initialized non-idle task, no interrupt-like context (including
/// BH-disabled sections), enabled local IRQs and, when tracked, no preemption
/// protection. Explicit RCU read scopes must also have ended. Call before
/// acquiring wait-path spinlocks. This is a snapshot,
/// not permission across later context changes; it does not detect application
/// dependency cycles or held sleepable locks.
pub fn is_sleepable_context() -> bool {
    let Some(task) = current_may_uninit() else {
        return false;
    };
    can_sleep_task_context(&task)
}

#[cfg(unittest)]
pub fn can_sleep_current_context_for_tests() -> bool {
    is_sleepable_context()
}

/// Exits the current task.
pub fn exit(exit_code: i32) -> ! {
    current_run_queue::<NoPreemptIrqSave>().exit_current(exit_code)
}

fn idle_wait_if_no_work(_guard: &NoPreemptIrqSave) -> bool {
    if current().is_preempt_pending() {
        return false;
    }
    debug_assert!(!current().rcu_reader.is_reading());
    debug_assert!(!kirq::context::is_in_interrupt_context());
    // Finish membership before entering a possibly tickless quiescent window.
    let task = current();
    // SAFETY: the current task is retained under the caller's IRQ/preemption
    // guard at this watched, non-NMI idle boundary, with no reader-node lock.
    let cleanup = unsafe { krcu_core::ReaderCleanup::new(&task.rcu_reader) };
    krcu_core::RcuContextIf::finish_reader_cleanup(cleanup);
    let watching = krcu_core::RcuContextIf::current_cpu_watching();
    // SAFETY: Only the idle loop and scope-free task tests call this helper.
    // The guard pins this CPU through restoration; no protected reference or
    // interrupt scope spans the wait. IRQ/NMI dispatch balances its activation,
    // and the outer preemption guard prevents scheduling in the IRQ return tail.
    unsafe { watching.enter_quiescent() };
    // SAFETY: The borrowed guard excludes preemption and normal IRQs. The
    // callers hold no runqueue lock and retain the ordinary task NMI state.
    unsafe { karch::idle_wait() };
    // SAFETY: idle_wait returns on this CPU with IRQs masked and all interrupt
    // entries balanced. Restore watching before guard release or bookkeeping.
    unsafe { watching.exit_quiescent() };
    true
}

#[cfg(unittest)]
unittest::path_mod_test!(tests_idle, "../ut-tests/tests/idle.rs");

/// The idle task routine.
///
/// Rechecks retained scheduling work with IRQs masked before each wait.
pub fn run_idle() -> ! {
    loop {
        yield_now();
        // Like Linux's idle loop, finish local bookkeeping before scheduling.
        // Keep this separate from IRQ exclusion: timer repair also logs and
        // notifies other CPUs, and need not extend the IRQ-off wait window.
        #[cfg(feature = "arm-timer-resume-fixup")]
        let _idle_pin = kspin::NoPreempt::new();
        let guard = NoPreemptIrqSave::new();
        #[cfg(feature = "arm-timer-resume-fixup")]
        let cntpct_before = khal::time::now_ticks();
        if !idle_wait_if_no_work(&guard) {
            continue;
        }
        drop(guard);

        #[cfg(feature = "arm-timer-resume-fixup")]
        {
            // Backend reads already publish repaired logical time. A second
            // logical-time comparison cannot diagnose a raw counter reset.
            let _ = khal::time::handle_idle_return(cntpct_before);
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

unittest::path_mod_test!(tests_api, "../ut-tests/api/tests_api.rs");

/// Returns `true` when no suspicious long lock-waits are observed on this CPU.
/// Returns `false` when a task appears to have been waiting on a lock for too long.
///
/// Note: this is a *heuristic* watchdog check, not a full deadlock detector.
/// The current time is sampled after each task's wait snapshot. A snapshot
/// whose start time is still in the future is skipped as a concurrent update.
#[cfg(feature = "watchdog")]
pub fn check_mutex_deadlock() -> bool {
    let mut ok = true;
    crate::task_registry::for_each_tracked_task(khal::percpu::this_cpu_id(), |weaktask| {
        if !ok {
            return;
        }
        if let Some(task) = weaktask.upgrade() {
            let Some((_lock, since)) = task.inner().waiting_snapshot() else {
                return;
            };

            let now = khal::time::now_ticks();
            let Some(blocked_ticks) = now.as_raw().checked_sub(since.as_raw()) else {
                return;
            };
            let blocked = khal::time::TimerTicks::from_raw(blocked_ticks);
            if khal::time::ticks_to_span(blocked) > ktime_types::TimeSpan::from_secs(20) {
                // suspect stall (20s)
                ok = false;
            }
        }
    });
    ok
}

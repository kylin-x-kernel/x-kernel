// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-CPU run queue implementation and scheduling helpers.

#[cfg(feature = "smp")]
use alloc::sync::Weak;
use alloc::{collections::VecDeque, sync::Arc};
use core::{
    future::poll_fn,
    ptr::NonNull,
    task::{Context, Poll},
};

use futures_util::task::AtomicWaker;
use kcpu_id_map::{KCpuMaskExt, LogicalCpuId};
use khal::percpu::this_cpu_id;
use klazy::Once;
use ksched::BaseScheduler;
use kspin::{BaseGuard, SpinNoIrqGuard, SpinRaw};
use lazyinit::LazyInit;

use crate::{
    KCpuMask, KtaskRef, Scheduler, TaskInner,
    future::block_on,
    task::{CurrentTask, TaskState},
    tracing_hooks::{fire_context_switch, fire_task_wakeup},
};

macro_rules! percpu_static {
    ($(
        $(#[$comment:meta])*
        $name:ident: $ty:ty = $init:expr
    ),* $(,)?) => {
        $(
            $(#[$comment])*
            #[percpu::def_percpu]
            static $name: $ty = $init;
        )*
    };
}

percpu_static! {
    RUN_QUEUE: LazyInit<RunQueue> = LazyInit::new(),
    EXITED_TASKS: VecDeque<KtaskRef> = VecDeque::new(),
    WAIT_FOR_EXIT: AtomicWaker = AtomicWaker::new(),
    IDLE_TASK: LazyInit<KtaskRef> = LazyInit::new(),
    /// Stores the weak reference to the previous task that is running on this CPU.
    #[cfg(feature = "smp")]
    PREV_TASK: Weak<crate::KTask> = Weak::new(),
}

struct RunQueueRegistry([Once<usize>; kbuild_config::NR_CPUS]);

impl RunQueueRegistry {
    const fn new() -> Self {
        Self([const { Once::new() }; kbuild_config::NR_CPUS])
    }

    fn register_current(&self, cpu_id: LogicalCpuId, run_queue: NonNull<RunQueue>) {
        self.0[cpu_id.as_usize()].call_once(|| run_queue.as_ptr().addr());
    }

    #[cfg(feature = "smp")]
    fn get(&self, index: usize) -> &'static mut RunQueue {
        let run_queue = *self.0[index]
            .get()
            .expect("run queue must be registered before cross-CPU lookup");
        // SAFETY:
        // - each slot is initialized exactly once during per-CPU scheduler
        //   bring-up before remote scheduling can reference it;
        // - the stored address points at a per-CPU `RunQueue` whose lifetime
        //   is the entire kernel run;
        // - callers use the returned reference under the scheduler's existing
        //   CPU ownership and IRQ/preemption exclusion rules.
        unsafe { &mut *(run_queue as *mut RunQueue) }
    }
}

static RUN_QUEUES: RunQueueRegistry = RunQueueRegistry::new();

#[inline(always)]
fn current_run_queue_ptr() -> NonNull<RunQueue> {
    // SAFETY: the current CPU's `RUN_QUEUE` percpu slot is initialized during
    // scheduler bring-up before normal scheduling paths run. This helper takes
    // the address of the initialized inner `RunQueue`, not the outer
    // `LazyInit<RunQueue>` wrapper.
    unsafe { NonNull::from(RUN_QUEUE.current_ref_raw().get_unchecked()) }
}

#[inline(always)]
fn current_run_queue_mut() -> &'static mut RunQueue {
    // SAFETY: the current CPU's `RUN_QUEUE` percpu slot is initialized before
    // scheduling code runs, and callers obtain mutable access only within the
    // scheduler's current-CPU ownership rules.
    unsafe { RUN_QUEUE.current_ref_mut_raw().get_mut_unchecked() }
}

#[inline(always)]
fn current_exited_tasks_mut() -> &'static mut VecDeque<KtaskRef> {
    // SAFETY: callers use this only while operating on the current CPU's
    // scheduler state, where the percpu exited-task list has been initialized.
    unsafe { EXITED_TASKS.current_ref_mut_raw() }
}

#[inline(always)]
fn current_wait_for_exit() -> &'static AtomicWaker {
    // SAFETY: the current CPU's percpu `WAIT_FOR_EXIT` waker is initialized
    // during scheduler bring-up before GC or exit paths use it.
    unsafe { WAIT_FOR_EXIT.current_ref_raw() }
}

#[inline(always)]
fn current_idle_task() -> &'static KtaskRef {
    // SAFETY: the current CPU's idle-task percpu slot is initialized during
    // scheduler bring-up before reschedule paths can use it.
    unsafe { IDLE_TASK.current_ref_raw().get_unchecked() }
}

#[cfg(feature = "smp")]
#[inline(always)]
fn current_prev_task_mut() -> &'static mut Weak<crate::KTask> {
    // SAFETY: callers touch `PREV_TASK` only while scheduling on the current
    // CPU with IRQs disabled, so the current CPU's percpu slot is initialized
    // and accessed under the scheduler's ownership rules.
    unsafe { PREV_TASK.current_ref_mut_raw() }
}

/// Returns a reference to the current run queue in [`CurrentRunQueueRef`].
///
/// ## Safety
///
/// This function returns a static reference to the current run queue, which
/// is inherently unsafe. It assumes that the `RUN_QUEUE` has been properly
/// initialized and is not accessed concurrently in a way that could cause
/// data races or undefined behavior.
///
/// ## Returns
///
/// * [`CurrentRunQueueRef`] - a static reference to the current [`RunQueue`].
#[inline(always)]
pub(crate) fn current_run_queue<G: BaseGuard>() -> CurrentRunQueueRef<'static, G> {
    let irq_state = G::acquire();
    CurrentRunQueueRef {
        inner: current_run_queue_mut(),
        current_task: crate::current(),
        state: irq_state,
        _phantom: core::marker::PhantomData,
    }
}

/// Selects the run queue index based on a CPU set bitmap and load balancing.
///
/// This function filters the available run queues based on the provided `cpumask` and
/// selects the run queue index for the next task. The selection is based on a round-robin algorithm.
///
/// ## Arguments
///
/// * `cpumask` - A bitmap representing the CPUs that are eligible for task execution.
///
/// ## Returns
///
/// The index (cpu_id) of the selected run queue.
///
/// ## Panics
///
/// This function will panic if `cpu_mask` is empty, indicating that there are no available CPUs for task execution.
#[cfg(feature = "smp")]
#[inline]
fn select_run_queue_index(cpumask: &KCpuMask) -> usize {
    use core::sync::atomic::{AtomicUsize, Ordering};
    static RUN_QUEUE_INDEX: AtomicUsize = AtomicUsize::new(0);

    assert!(!cpumask.is_empty(), "No available CPU for task execution");

    // Collect eligible CPU indices into a small stack buffer and round-robin
    // over them directly. This avoids scanning NR_CPUS-sized ranges when only
    // a subset of CPUs are present and eligible.
    let mut eligible = [0usize; kbuild_config::NR_CPUS];
    let mut count = 0usize;
    for cpu_id in cpumask.iter_logical() {
        eligible[count] = cpu_id.as_usize();
        count += 1;
    }
    let nth = RUN_QUEUE_INDEX.fetch_add(1, Ordering::Relaxed) % count;
    eligible[nth]
}

/// Retrieves a `'static` reference to the run queue corresponding to the given index.
///
/// This function asserts that the provided index is within the range of available CPUs
/// and returns a reference to the corresponding run queue.
///
/// ## Arguments
///
/// * `index` - The index of the run queue to retrieve.
///
/// ## Returns
///
/// A reference to the `RunQueue` corresponding to the provided index.
///
/// ## Panics
///
/// This function will panic if the index is out of bounds.
#[cfg(feature = "smp")]
#[inline]
fn get_run_queue(index: usize) -> &'static mut RunQueue {
    RUN_QUEUES.get(index)
}

/// Selects the appropriate run queue for the provided task.
///
/// * In a single-core system, this function always returns a reference to the global run queue.
/// * In a multi-core system, this function selects the run queue based on the task's CPU affinity and load balance.
///
/// ## Arguments
///
/// * `task` - A reference to the task for which a run queue is being selected.
///
/// ## Returns
///
/// * [`KRunQueueRef`] - a static reference to the selected [`RunQueue`] (current or remote).
///
/// ## TODO
///
/// 1. Implement better load balancing across CPUs for more efficient task distribution.
/// 2. Use a more generic load balancing algorithm that can be customized or replaced.
#[inline]
pub(crate) fn select_run_queue<G: BaseGuard>(task: &KtaskRef) -> KRunQueueRef<'static, G> {
    let irq_state = G::acquire();
    #[cfg(not(feature = "smp"))]
    {
        let _ = task;
        // When SMP is disabled, all tasks are scheduled on the same global run queue.
        KRunQueueRef {
            inner: current_run_queue_mut(),
            state: irq_state,
            _phantom: core::marker::PhantomData,
        }
    }
    #[cfg(feature = "smp")]
    {
        // When SMP is enabled, select the run queue based on the task's CPU affinity and load balance.
        let index = select_run_queue_index(&task.cpumask());
        KRunQueueRef {
            inner: get_run_queue(index),
            state: irq_state,
            _phantom: core::marker::PhantomData,
        }
    }
}

/// Selects a run queue for a task that is becoming runnable from a wakeup.
#[inline]
pub(crate) fn select_wake_run_queue<G: BaseGuard>(task: &KtaskRef) -> KRunQueueRef<'static, G> {
    let irq_state = G::acquire();
    #[cfg(not(feature = "smp"))]
    {
        let _ = task;
        KRunQueueRef {
            inner: current_run_queue_mut(),
            state: irq_state,
            _phantom: core::marker::PhantomData,
        }
    }
    #[cfg(feature = "smp")]
    {
        // Prefer the CPU where the task blocked; fall back to RR if affinity changed.
        let last_cpu = task.cpu_id();
        let index = if task.cpumask().get(last_cpu.as_usize()) {
            last_cpu.as_usize()
        } else {
            select_run_queue_index(&task.cpumask())
        };
        KRunQueueRef {
            inner: get_run_queue(index),
            state: irq_state,
            _phantom: core::marker::PhantomData,
        }
    }
}

#[inline]
pub(crate) fn task_run_queue<G: BaseGuard>(task: &KtaskRef) -> KRunQueueRef<'static, G> {
    let irq_state = G::acquire();
    #[cfg(not(feature = "smp"))]
    {
        let _ = task;
        KRunQueueRef {
            inner: current_run_queue_mut(),
            state: irq_state,
            _phantom: core::marker::PhantomData,
        }
    }
    #[cfg(feature = "smp")]
    {
        KRunQueueRef {
            inner: get_run_queue(task.cpu_id().as_usize()),
            state: irq_state,
            _phantom: core::marker::PhantomData,
        }
    }
}

#[cfg(feature = "preempt")]
fn request_resched(cpu_id: LogicalCpuId) {
    if cpu_id == this_cpu_id() {
        crate::current().set_preempt_pending(true);
    } else {
        #[cfg(all(feature = "smp", feature = "ipi"))]
        request_remote_resched(cpu_id);
    }
}

#[cfg(all(feature = "preempt", feature = "smp", feature = "ipi"))]
fn request_remote_resched(cpu_id: LogicalCpuId) {
    if let Err(err) = kipi::run_on_cpu(cpu_id, || {
        crate::current().set_preempt_pending(true);
    }) {
        warn!(
            "failed to request reschedule on CPU {}: {}",
            cpu_id.as_usize(),
            err
        );
    }
}

/// [`RunQueue`] represents a run queue for global system or a specific CPU.
pub(crate) struct RunQueue {
    /// The ID of the CPU this run queue is associated with.
    cpu_id: LogicalCpuId,
    /// The core scheduler of this run queue.
    /// Since irq and preempt are preserved by the kernel guard hold by `KRunQueueRef`,
    /// we just use a simple raw spin lock here.
    scheduler: SpinRaw<Scheduler>,
}

/// A reference to the run queue with specific guard.
///
/// Note:
/// [`KRunQueueRef`] is used to get a reference to the run queue on current CPU
/// or a remote CPU, which is used to add tasks to the run queue or unblock tasks.
/// If you want to perform scheduling operations on the current run queue,
/// see [`CurrentRunQueueRef`].
pub(crate) struct KRunQueueRef<'a, G: BaseGuard> {
    inner: &'a mut RunQueue,
    state: G::State,
    _phantom: core::marker::PhantomData<G>,
}

impl<G: BaseGuard> Drop for KRunQueueRef<'_, G> {
    fn drop(&mut self) {
        G::release(self.state);
    }
}

/// A reference to the current run queue with specific guard.
///
/// Note:
/// [`CurrentRunQueueRef`] is used to get a reference to the run queue on current CPU,
/// in which scheduling operations can be performed.
pub(crate) struct CurrentRunQueueRef<'a, G: BaseGuard> {
    inner: &'a mut RunQueue,
    current_task: CurrentTask,
    state: G::State,
    _phantom: core::marker::PhantomData<G>,
}

impl<G: BaseGuard> Drop for CurrentRunQueueRef<'_, G> {
    fn drop(&mut self) {
        G::release(self.state);
    }
}

/// Management operations for run queue, including adding tasks, unblocking tasks, etc.
impl<G: BaseGuard> KRunQueueRef<'_, G> {
    /// Adds a task to the scheduler.
    ///
    /// This function is used to add a new task to the scheduler.
    pub fn add_task(&mut self, task: KtaskRef) {
        debug!(
            "task add: {} on run_queue {}",
            task.id_name(),
            self.inner.cpu_id.as_usize()
        );
        assert!(task.is_ready());
        #[cfg(feature = "snapshot")]
        {
            let _g = kspin::NoPreempt::new();
            crate::task_registry::record_tracked_task(&task);
        }
        self.inner.scheduler.lock().add_task(task);
    }

    /// Unblock one task by inserting it into the run queue.
    ///
    /// This function does nothing if the task is not in [`TaskState::Blocked`],
    /// which means the task is already unblocked by other cores.
    pub fn unblock_task(&mut self, task: KtaskRef, resched: bool) {
        let task_id_name = task.id_name();
        let task_id = task.trace_id();
        // Try to change the state of the task from `Blocked` to `Ready`,
        // if successful, the task will be put into this run queue,
        // otherwise, the task is already unblocked by other cores.
        // Note:
        // target task can not be insert into the run queue until it finishes its scheduling process.
        if self
            .inner
            .put_task_with_state(task, TaskState::Blocked, false)
        {
            // Since now, the task to be unblocked is in the `Ready` state.
            let cpu_id = self.inner.cpu_id;
            debug!(
                "task unblock: {task_id_name} on run_queue {}",
                cpu_id.as_usize()
            );

            // Fire the task wakeup tracepoint.
            fire_task_wakeup(task_id);

            #[cfg(feature = "preempt")]
            if resched {
                request_resched(cpu_id);
            }
        }
    }
}

/// Core functions of run queue.
impl<G: BaseGuard> CurrentRunQueueRef<'_, G> {
    pub fn scheduler_timer_tick(&mut self) {
        let curr = &self.current_task;
        if !curr.is_idle() && self.inner.scheduler.lock().task_tick(curr) {
            #[cfg(feature = "preempt")]
            curr.set_preempt_pending(true);
        }
    }

    /// Yield the current task and reschedule.
    /// This function will put the current task into this run queue with `Ready` state,
    /// and reschedule to the next task on this run queue.
    pub fn yield_current(&mut self) {
        let curr = &self.current_task;
        trace!("task yield: {}", curr.id_name());
        assert!(curr.is_running());

        self.inner
            .put_task_with_state(curr.clone(), TaskState::Running, false);

        self.inner.resched();
    }

    /// Migrate the current task to a new run queue matching its CPU affinity and reschedule.
    /// This function will spawn a new `migration_task` to perform the migration, which will set
    /// current task to `Ready` state and select a proper run queue for it according to its CPU affinity,
    /// switch to the migration task immediately after migration task is prepared.
    ///
    /// Note: the ownership if migrating task (which is current task) is handed over to the migration task,
    /// before the migration task inserted it into the target run queue.
    #[cfg(feature = "smp")]
    pub fn migrate_current(&mut self, migration_task: KtaskRef) {
        let curr = &self.current_task;
        trace!("task migrate: {}", curr.id_name());
        assert!(curr.is_running());

        // Mark current task's state as `Ready`,
        // but, do not put current task to the scheduler of this run queue.
        curr.set_state(TaskState::Ready);

        // Call `switch_to` to reschedule to the migration task that performs the migration directly.
        self.inner.switch_to(crate::current(), migration_task);
    }

    /// Preempts the current task and reschedules.
    /// This function is used to preempt the current task and reschedule
    /// to next task on current run queue.
    ///
    /// This function is called by `current_check_preempt_pending` with IRQs and preemption disabled.
    ///
    /// Note:
    /// preemption may happened in `enable_preempt`, which is called
    /// each time a [`kspin::NoPreemptGuard`] is dropped.
    #[cfg(feature = "preempt")]
    pub fn preempt_resched(&mut self) {
        // There is no need to disable IRQ and preemption here, because
        // they both have been disabled in `current_check_preempt_pending`.
        let curr = &self.current_task;
        assert!(curr.is_running());

        // When we call `preempt_resched()`, both IRQs and preemption must
        // have been disabled by `kspin::NoPreemptIrqSave`. So we need
        // to set `current_disable_count` to 1 in `can_preempt()` to obtain
        // the preemption permission.
        let can_preempt = curr.can_preempt(1);

        trace!(
            "current task is to be preempted: {}, allow={}",
            curr.id_name(),
            can_preempt
        );
        if can_preempt {
            self.inner
                .put_task_with_state(curr.clone(), TaskState::Running, true);
            self.inner.resched();
        } else {
            curr.set_preempt_pending(true);
        }
    }

    /// Exit the current task with the specified exit code.
    /// This function will never return.
    pub fn exit_current(&mut self, exit_code: i32) -> ! {
        let curr = &self.current_task;
        debug!("task exit: {}, exit_code={}", curr.id_name(), exit_code);
        assert!(curr.is_running(), "task is not running: {:?}", curr.state());
        assert!(!curr.is_idle());
        if curr.is_init() {
            // SAFETY: `exit_current` runs under
            // `current_run_queue::<NoPreemptIrqSave>()`, so IRQs and
            // preemption are disabled while touching the current CPU's percpu
            // exited-task list.
            current_exited_tasks_mut().clear();
            khal::power::shutdown();
        } else {
            // Notify the joiner task.
            curr.notify_exit(exit_code);

            // SAFETY: `exit_current` runs under
            // `current_run_queue::<NoPreemptIrqSave>()`, so IRQs and
            // preemption are disabled while touching current-CPU percpu
            // scheduler queues and wakers.
            // Push current task to the `EXITED_TASKS` list, which will be consumed by the GC task.
            current_exited_tasks_mut().push_back(curr.clone());
            // Wake up the GC task to drop the exited tasks.
            current_wait_for_exit().wake();

            // Schedule to next task.
            self.inner.resched();
        }
        unreachable!("task exited!");
    }

    /// Block the current task, put current task into the wait queue and reschedule.
    /// Mark the state of current task as `Blocked`, set the `in_wait_queue` flag as true.
    /// Note:
    ///     1. The caller must hold the lock of the wait queue.
    ///     2. The caller must ensure that the current task is in the running state.
    ///     3. The caller must ensure that the current task is not the idle task.
    ///     4. The lock of the wait queue will be released explicitly after current task is pushed into it.
    pub fn blocked_resched(&mut self, mut woke: SpinNoIrqGuard<'_, bool>) {
        let curr = &self.current_task;
        assert!(curr.is_running());
        assert!(!curr.is_idle());
        // we must not block current task with preemption disabled.
        // Current expected preempt count is 2 for `NoPreemptIrqSave` because we also hold
        // the `woke` SpinNoIrqGuard lock here.
        #[cfg(feature = "preempt")]
        assert!(curr.can_preempt(2));

        // Mark the task as blocked, this has to be done before adding it to the wait queue
        // while holding the lock of the wait queue.
        curr.set_state(TaskState::Blocked);
        *woke = false;
        drop(woke);

        // Current task's state has been changed to `Blocked` and added to the wait queue.
        // Note that the state may have been set as `Ready` in `unblock_task()`,
        // see `unblock_task()` for details.

        debug!("task block: {}", curr.id_name());
        self.inner.resched();
    }

    pub fn set_current_priority(&mut self, prio: isize) -> bool {
        self.inner
            .scheduler
            .lock()
            .set_priority(&self.current_task, prio)
    }
}

impl<G: BaseGuard> KRunQueueRef<'_, G> {
    pub fn set_task_priority(&mut self, task: &KtaskRef, prio: isize) -> bool {
        self.inner.scheduler.lock().set_priority(task, prio)
    }
}

impl RunQueue {
    /// Create a new run queue for the specified CPU.
    /// The run queue is initialized with a per-CPU gc task in its scheduler.
    fn new(cpu_id: LogicalCpuId) -> Self {
        let gc_task = TaskInner::new_internal(
            || block_on(poll_fn(poll_gc)),
            "gc".into(),
            kbuild_config::TASK_STACK_SIZE,
        )
        .into_arc();
        // gc task should be pinned to the current CPU.
        gc_task.set_cpumask(KCpuMask::one_shot_logical(cpu_id));

        let mut scheduler = Scheduler::new();
        scheduler.add_task(gc_task);
        Self {
            cpu_id,
            scheduler: SpinRaw::new(scheduler),
        }
    }

    /// Puts target task into current run queue with `Ready` state
    /// if its state matches `current_state` (except idle task).
    ///
    /// If `preempt`, keep current task's time slice, otherwise reset it.
    ///
    /// Returns `true` if the target task is put into this run queue successfully,
    /// otherwise `false`.
    fn put_task_with_state(
        &mut self,
        task: KtaskRef,
        current_state: TaskState,
        preempt: bool,
    ) -> bool {
        // If the task's state matches `current_state`, set its state to `Ready` and
        // put it back to the run queue (except idle task).
        if task.transition_state(current_state, TaskState::Ready) && !task.is_idle() {
            // If the task is blocked, wait for the task to finish its scheduling process.
            // See `unblock_task()` for details.
            if current_state == TaskState::Blocked {
                // Wait for next task's scheduling process to complete.
                // If the owning (remote) CPU is still in the middle of schedule() with
                // this task (next task) as prev, wait until it's done referencing the task.
                //
                // Pairs with the `clear_prev_task_on_cpu()`.
                //
                // Note:
                // 1. This should be placed after the judgement of `TaskState::Blocked,`,
                //    because the task may have been woken up by other cores.
                // 2. This can be placed in the front of `switch_to()`
                #[cfg(feature = "smp")]
                while task.on_cpu() {
                    // Wait for the task to finish its scheduling process.
                    core::hint::spin_loop();
                }
            }
            // TODO: priority
            #[cfg(feature = "smp")]
            task.set_cpu_id(self.cpu_id);
            self.scheduler.lock().put_prev_task(task, preempt);
            true
        } else {
            false
        }
    }

    /// Core reschedule subroutine.
    /// Pick the next task to run and switch to it.
    fn resched(&mut self) {
        let next = self
            .scheduler
            .lock()
            .pick_next_task()
            .unwrap_or_else(|| current_idle_task().clone());
        assert!(
            next.is_ready(),
            "next {} is not ready: {:?}",
            next.id_name(),
            next.state()
        );
        self.switch_to(crate::current(), next);
    }

    fn switch_to(&mut self, prev_task: CurrentTask, next_task: KtaskRef) {
        // Make sure that IRQs are disabled by kernel guard or other means.
        assert!(
            !karch::local_irq_enabled(),
            "IRQs must be disabled during scheduling"
        );
        trace!(
            "context switch: {} -> {}",
            prev_task.id_name(),
            next_task.id_name()
        );
        #[cfg(feature = "preempt")]
        next_task.set_preempt_pending(false);
        next_task.set_state(TaskState::Running);
        if prev_task.ptr_eq(&next_task) {
            return;
        }

        // Fire the context switch tracepoint.
        fire_context_switch(prev_task.trace_id(), next_task.trace_id());

        // Claim the task as running, we do this before switching to it
        // such that any running task will have this set.
        #[cfg(feature = "smp")]
        {
            next_task.set_on_cpu(true);
            next_task.set_on_cpu_mask_bit(this_cpu_id());
        }

        use crate::TaskExt;
        let cpu_id = this_cpu_id();
        if let Some(ext) = next_task.task_ext() {
            // Publish the CPU in the next mm before switching hardware state
            // so concurrent flushers can over-target but never miss this CPU.
            // We intentionally do not clear the previous mm here: code after
            // `switch_to()` runs only when the old task is scheduled again, so
            // eager clear-after-switch is not a sound timing point.
            ext.set_user_mm_resident_cpu(cpu_id);
        }

        {
            use crate::TaskExt;

            if let Some(ext) = prev_task.task_ext() {
                ext.on_leave()
            }
            if let Some(ext) = next_task.task_ext() {
                ext.on_enter()
            }
        }

        // SAFETY: scheduling owns both task contexts here, IRQs are disabled,
        // and the percpu scheduler/task pointers being updated are local to
        // the current CPU.
        unsafe {
            let prev_ctx_ptr = prev_task.ctx_mut_ptr();
            let next_ctx_ptr = next_task.ctx_mut_ptr();

            #[cfg(target_arch = "aarch64")]
            if let Some(root) = next_task
                .task_ext()
                .and_then(|ext| ext.switch_page_table_root())
            {
                (*next_ctx_ptr).set_page_table_root(root);
            }

            // Store the weak pointer of **prev_task** in percpu variable `PREV_TASK`.
            #[cfg(feature = "smp")]
            {
                *current_prev_task_mut() = Arc::downgrade(&prev_task);
            }

            // The strong reference count of `prev_task` will be decremented by 1,
            // but won't be dropped until `gc_entry()` is called.
            assert!(Arc::strong_count(&prev_task) > 1);
            assert!(Arc::strong_count(&next_task) >= 1);

            CurrentTask::set_current(prev_task, next_task);

            (*prev_ctx_ptr).switch_to(&*next_ctx_ptr);

            // Current it's **next_task** running on this CPU, clear the `prev_task`'s `on_cpu` field
            // to indicate that it has finished its scheduling process and no longer running on this CPU.
            #[cfg(feature = "smp")]
            clear_prev_task_on_cpu();
        }
    }
}

fn poll_gc(cx: &mut Context<'_>) -> Poll<()> {
    loop {
        // Drop all exited tasks and recycle resources.
        let n = EXITED_TASKS.with_current(|exited_tasks| exited_tasks.len());
        for _ in 0..n {
            // Do not do the slow drops in the critical section.
            let Some(task) = EXITED_TASKS.with_current(|exited_tasks| exited_tasks.pop_front())
            else {
                continue;
            };
            match Arc::try_unwrap(task) {
                Ok(task) => {
                    // If I'm the last holder of the task, drop it immediately.
                    drop(task);
                }
                Err(task) => {
                    // Otherwise (e.g, `switch_to` is not completed, held by the
                    // joiner, etc), push it back and wait for them to drop first.
                    EXITED_TASKS.with_current(|exited_tasks| exited_tasks.push_back(task));
                }
            }
        }

        #[cfg(feature = "snapshot")]
        {
            let _g = kspin::NoPreempt::new();
            crate::task_registry::sweep_tracked_tasks(this_cpu_id());
        }

        // Note: we cannot block current task with preemption disabled,
        // use `current_ref_raw` to get the `WAIT_FOR_EXIT`'s reference here to avoid
        // the use of `NoPreemptGuard`. Since gc task is pinned to the current
        // CPU, there is no affection if the gc task is preempted during the process.
        current_wait_for_exit().register(cx.waker());

        // New tasks might be added during the above section, recheck it to
        // prevent us from sleeping indefinitely.
        if EXITED_TASKS.with_current(|exited_tasks| exited_tasks.is_empty()) {
            break;
        }

        crate::yield_now();
    }

    Poll::Pending
}

/// The task routine for migrating the current task to the correct CPU.
///
/// It calls `select_run_queue` to get the correct run queue for the task, and
/// then puts the task to the scheduler of target run queue.
#[cfg(feature = "smp")]
pub(crate) fn migrate_entry(migrated_task: KtaskRef) {
    select_run_queue::<kspin::NoPreemptIrqSave>(&migrated_task)
        .inner
        .scheduler
        .lock()
        .put_prev_task(migrated_task, false)
}

/// Clear the `on_cpu` field of previous task running on this CPU.
#[cfg(feature = "smp")]
pub(crate) unsafe fn clear_prev_task_on_cpu() {
    // SAFETY: this is called on the CPU that owns `PREV_TASK`, after the
    // context switch completed and before another previous-task record is
    // installed.
    unsafe {
        PREV_TASK
            .current_ref_raw()
            .upgrade()
            .expect("Invalid prev_task pointer or prev_task has been dropped")
            .set_on_cpu(false);
    }
}
pub(crate) fn init() {
    let cpu_id = this_cpu_id();

    // Create the `idle` task (not current task).
    // The idle task will run when there is no other runnable task.
    // Stack size of idle task should be large because traps/interrupts may happen in idle task,
    // which need more stack space.
    const IDLE_TASK_STACK_SIZE: usize = 16384;
    let idle_task = TaskInner::new_idle(|| crate::run_idle(), "idle".into(), IDLE_TASK_STACK_SIZE);
    // idle task should be pinned to the current CPU.
    idle_task.set_cpumask(KCpuMask::one_shot_logical(cpu_id));
    IDLE_TASK.with_current(|i| {
        i.init_once(idle_task.into_arc());
    });

    // Put the subsequent execution into the `main` task.
    let main_task = TaskInner::new_boot("main".into()).into_arc();
    main_task.set_state(TaskState::Running);
    #[cfg(feature = "smp")]
    main_task.set_cpu_id(cpu_id);
    #[cfg(feature = "snapshot")]
    crate::task_registry::record_tracked_task(&main_task);
    // SAFETY: scheduler bring-up installs the first current task for this CPU
    // before any concurrent task access can occur.
    unsafe { CurrentTask::init_current(main_task) }

    RUN_QUEUE.with_current(|rq| {
        rq.init_once(RunQueue::new(cpu_id));
    });
    RUN_QUEUES.register_current(cpu_id, current_run_queue_ptr());
}

pub(crate) fn init_secondary() {
    let cpu_id = this_cpu_id();

    // Put the subsequent execution into the `idle` task.
    let idle_task = TaskInner::new_current_idle("idle".into()).into_arc();
    idle_task.set_state(TaskState::Running);
    #[cfg(feature = "smp")]
    idle_task.set_cpu_id(cpu_id);
    #[cfg(feature = "snapshot")]
    crate::task_registry::record_tracked_task(&idle_task);
    IDLE_TASK.with_current(|i| {
        i.init_once(idle_task.clone());
    });
    // SAFETY: scheduler bring-up installs the first current task for this CPU
    // before any concurrent task access can occur.
    unsafe { CurrentTask::init_current(idle_task) }

    RUN_QUEUE.with_current(|rq| {
        rq.init_once(RunQueue::new(cpu_id));
    });
    RUN_QUEUES.register_current(cpu_id, current_run_queue_ptr());
}

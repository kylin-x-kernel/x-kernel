// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-CPU run queue: ownership, placement, switch, and delayed reclaim.
//!
//! Submodules keep SMP placement out of the local leave/pick path:
//! - `placement`: spawn idle-first, wake SIS, idle-pull source choice
//! - `rq`: leave / pick / switch / kick
//! - `ownership`: `cpu_id` and `nr_running` / `nr_home` snapshots
//! - `idle_pull`: steal a Ready `!on_cpu` waiter onto an about-to-idle CPU
//! - `migrate`: affinity enforcement after `set_cpumask`
//! - `gc`: delayed `Arc::try_unwrap` of exited tasks
//! - `stats`: optional `sched_stat` counters
//!
//! Callers still enter through `current_run_queue`, `select_run_queue`,
//! `select_wake_run_queue`, `task_run_queue`, and `enforce_affinity_placement`.
//! Placement reads remote `nr_running` / `nr_home` without taking a remote
//! scheduler lock; `ksched` only sees the already-chosen queue.

mod gc;
#[cfg(feature = "smp")]
mod idle_pull;
#[cfg(feature = "smp")]
mod migrate;
#[cfg(feature = "smp")]
mod ownership;
mod placement;
mod rq;
mod stats;

#[cfg(unittest)]
mod tests;

use alloc::collections::VecDeque;
#[cfg(feature = "smp")]
use alloc::sync::Weak;
use core::ptr::NonNull;
#[cfg(feature = "smp")]
use core::sync::atomic::AtomicUsize;

use kcpu_id_map::{KCpuMaskExt, LogicalCpuId};
use khal::percpu::this_cpu_id;
use klazy::Once;
use kpoll::PollSet;
use kspin::{BaseGuard, SpinRaw};
use lazyinit::LazyInit;
#[cfg(feature = "smp")]
pub(crate) use migrate::enforce_affinity_placement;
pub(crate) use placement::{select_run_queue, select_wake_run_queue, task_run_queue};
#[cfg(feature = "smp")]
pub(crate) use rq::clear_prev_task_on_cpu;
pub(crate) use rq::kick_running_task;
#[cfg(feature = "sched_stat")]
pub(crate) use stats::note_timer_irq;
pub(crate) use stats::{
    dump_sched_stats, record_preempt_pending_blocked, record_preempt_pending_check,
    sched_stats_text,
};

use crate::{
    KCpuMask, KtaskRef, Scheduler, TaskInner,
    api::current,
    task::{CurrentTask, TaskState},
};

percpu_static! {
    RUN_QUEUE: LazyInit<RunQueue> = LazyInit::new(),
    EXITED_TASKS: VecDeque<KtaskRef> = VecDeque::new(),
    WAIT_FOR_EXIT: LazyInit<PollSet> = LazyInit::new(),
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
        self.try_get(index)
            .expect("run queue must be registered before cross-CPU lookup")
    }

    /// Returns the run queue for `index` if that CPU has finished scheduler bring-up.
    #[cfg(feature = "smp")]
    fn try_get(&self, index: usize) -> Option<&'static mut RunQueue> {
        let run_queue = *self.0.get(index)?.get()?;
        // SAFETY:
        // - each slot is initialized exactly once during per-CPU scheduler
        //   bring-up before remote scheduling can reference it;
        // - the stored address points at a per-CPU `RunQueue` whose lifetime
        //   is the entire kernel run;
        // - callers use the returned reference under the scheduler's existing
        //   CPU ownership and IRQ/preemption exclusion rules.
        Some(unsafe { &mut *(run_queue as *mut RunQueue) })
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
pub(crate) fn current_run_queue_mut() -> &'static mut RunQueue {
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
fn current_wait_for_exit() -> &'static PollSet {
    // SAFETY: the current CPU's percpu `WAIT_FOR_EXIT` PollSet is initialized
    // during scheduler bring-up before GC or exit paths use it.
    unsafe { WAIT_FOR_EXIT.current_ref_raw().get_unchecked() }
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

/// Returns a guard over the current CPU's run queue.
///
/// Callers must run after scheduler bring-up. The `G` acquire/release pair
/// provides the IRQ/preempt exclusion that makes the `'static` borrow sound.
#[inline(always)]
pub(crate) fn current_run_queue<G: BaseGuard>() -> CurrentRunQueueRef<'static, G> {
    let irq_state = G::acquire();
    CurrentRunQueueRef {
        inner: current_run_queue_mut(),
        current_task: current(),
        state: irq_state,
        _phantom: core::marker::PhantomData,
    }
}
/// Looks up a brought-up run queue by logical CPU index.
///
/// ## Panics
///
/// Panics if that CPU has not registered a run queue yet.
#[cfg(feature = "smp")]
#[inline]
fn get_run_queue(index: usize) -> &'static mut RunQueue {
    RUN_QUEUES.get(index)
}
pub(crate) struct RunQueue {
    /// The ID of the CPU this run queue is associated with.
    cpu_id: LogicalCpuId,
    /// The core scheduler of this run queue.
    /// Since irq and preempt are preserved by the kernel guard hold by `KRunQueueRef`,
    /// we just use a simple raw spin lock here.
    scheduler: SpinRaw<Scheduler>,
    /// Last monotonic ns charged to the running task on this RQ.
    ///
    /// Always updated while holding [`Self::scheduler`]. Remote wakes and the
    /// local timer IRQ both take that lock, so they cannot double-count or drop
    /// a NOHZ gap before PLACE_LAG.
    last_accounted_ns: u64,
    /// Runnable-task snapshot (block discharges). Spawn idle-first looks at
    /// this: `nr_running == 0` outranks any busy CPU, including vs a sleeper's
    /// [`Self::nr_home`].
    ///
    /// Relaxed: heuristic only. Updated under [`Self::scheduler`] via
    /// [`TaskInner::mark_rq_load_charged`] / [`TaskInner::clear_rq_load_charged`].
    #[cfg(feature = "smp")]
    nr_running: AtomicUsize,
    /// Sticky-home occupancy: running, ready, **and** blocked tasks whose
    /// `cpu_id` still names this RQ. Second spawn key after idle-first, so
    /// two idle CPUs prefer the one with fewer sticky residents.
    #[cfg(feature = "smp")]
    nr_home: AtomicUsize,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum WakeEnqueue {
    Rejected,
    #[cfg(feature = "smp")]
    Deferred,
    Enqueued,
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

/// A reference to the current run queue with specific guard.
///
/// [`CurrentRunQueueRef`] is used to get a reference to the run queue on current CPU,
/// in which scheduling operations can be performed.
pub(crate) struct CurrentRunQueueRef<'a, G: BaseGuard> {
    inner: &'a mut RunQueue,
    current_task: CurrentTask,
    state: G::State,
    _phantom: core::marker::PhantomData<G>,
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
    #[cfg(feature = "smp")]
    RunQueue::set_owner_cpu(&idle_task, cpu_id);
    IDLE_TASK.with_current(|i| {
        i.init_once(idle_task.into_arc());
    });

    // Put the subsequent execution into the `main` task.
    let main_task = TaskInner::new_boot("main".into()).into_arc();
    main_task.set_state(TaskState::Running);
    #[cfg(feature = "smp")]
    RunQueue::set_owner_cpu(&main_task, cpu_id);
    #[cfg(feature = "snapshot")]
    crate::task_registry::record_tracked_task(&main_task);
    // SAFETY: scheduler bring-up installs the first current task for this CPU
    // before any concurrent task access can occur.
    unsafe { CurrentTask::init_current(main_task.clone()) }

    RUN_QUEUE.with_current(|rq| {
        rq.init_once(RunQueue::new(cpu_id));
    });
    RUN_QUEUES.register_current(cpu_id, current_run_queue_ptr());
    #[cfg(feature = "smp")]
    current_run_queue_mut().charge_rq_load(&main_task);
    #[cfg(feature = "smp")]
    current_run_queue_mut().charge_home(&main_task);
}

pub(crate) fn init_secondary() {
    let cpu_id = this_cpu_id();

    // Put the subsequent execution into the `idle` task.
    let idle_task = TaskInner::new_current_idle("idle".into()).into_arc();
    idle_task.set_state(TaskState::Running);
    #[cfg(feature = "smp")]
    RunQueue::set_owner_cpu(&idle_task, cpu_id);
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

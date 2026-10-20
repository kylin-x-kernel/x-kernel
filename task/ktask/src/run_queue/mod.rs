// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-CPU run queue: ownership, placement, switch, and delayed reclaim.
//!
//! Submodules keep SMP placement out of the local leave/pick path:
//! - `placement`: Fair spawn idle-first and wake SIS; RT uses `find_lowest_rt_cpu`
//! - `rq`: leave / pick / switch / kick
//! - `ownership`: `cpu_id` and `nr_running` / `nr_home` snapshots
//! - `idle_pull`: steal a Ready `!on_cpu` waiter onto an about-to-idle CPU
//! - `migrate`: affinity enforcement after `set_cpumask`
//! - `gc`: delayed `Arc::try_unwrap` of exited tasks
//! - `stats`: optional `sched_stat` counters
//!
//! Callers still enter through `current_run_queue`, `select_run_queue`,
//! `select_wake_run_queue`, and `enforce_affinity_placement`. Nice updates
//! use `set_task_nice`, which locks the owner queue itself.
//! Placement reads remote `nr_running` / `nr_home` / `rt_preempt_rank` without
//! taking a remote scheduler lock; `ksched` only sees the already-chosen queue.

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
use core::sync::atomic::{AtomicU8, AtomicUsize};
use core::{ptr::NonNull, sync::atomic::AtomicU64};

use kcpu_id_map::{KCpuMaskExt, LogicalCpuId};
use khal::percpu::this_cpu_id;
use klazy::Once;
use kpoll::PollSet;
use kspin::{BaseGuard, SpinRaw};
use lazyinit::LazyInit;
#[cfg(feature = "smp")]
pub(crate) use migrate::enforce_affinity_placement;
pub(crate) use placement::{select_run_queue, select_wake_run_queue};
pub(crate) use rq::{finish_task_switch, kick_running_task, set_task_nice};
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

// Scheduler payloads use allocation-free template markers (lazy-uninitialized,
// empty deque, None), so `StaticSlotValue`'s container blanket impls
// carry the template-copy proof; drops happen only through owner-code
// removal (switch cleanup takes `PREV_TASK`, GC pops drop deque entries),
// never through storage teardown.
// Macro-generated wrapper modules do not inherit the invocation site's
// lexical scope, so payload types are anchored absolutely.
// SAFETY: `LazyInit::new()` is an uninitialized marker with no owned payload.
kcpu_slot::cpu_slot_cell_with_template! {
    static RUN_QUEUE: lazyinit::LazyInit<crate::run_queue::RunQueue> =
        unsafe { lazyinit::LazyInit::new() };
}
// SAFETY: both the lock and deque are empty and own no allocation.
kcpu_slot::cpu_slot_cell_with_template! {
    static EXITED_TASKS: kspin::SpinNoIrq<alloc::collections::vec_deque::VecDeque<crate::KtaskRef>> =
        unsafe { kspin::SpinNoIrq::new(alloc::collections::vec_deque::VecDeque::new()) };
}
// SAFETY: `LazyInit::new()` is an uninitialized marker with no owned payload.
kcpu_slot::cpu_slot_cell_with_template! {
    static WAIT_FOR_EXIT: lazyinit::LazyInit<kpoll::PollSet> = unsafe { lazyinit::LazyInit::new() };
}
// SAFETY: `LazyInit::new()` is an uninitialized marker with no owned payload.
kcpu_slot::cpu_slot_cell_with_template! {
    static IDLE_TASK: lazyinit::LazyInit<crate::KtaskRef> =
        unsafe { lazyinit::LazyInit::new() };
}
// SAFETY: the lock is unlocked and `None` owns no allocation.
kcpu_slot::cpu_slot_cell_with_template! {
    /// Owns the outgoing current reference until the incoming task finishes
    /// the switch on this CPU, including on a uniprocessor.
    static PREV_TASK: kspin::SpinNoIrq<Option<crate::KtaskRef>> =
        unsafe { kspin::SpinNoIrq::new(None) };
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
    fn get(&self, index: usize) -> &'static RunQueue {
        self.try_get(index)
            .expect("run queue must be registered before cross-CPU lookup")
    }

    /// Returns the run queue for `index` if that CPU has finished scheduler bring-up.
    #[cfg(feature = "smp")]
    fn try_get(&self, index: usize) -> Option<&'static RunQueue> {
        let run_queue = *self.0.get(index)?.get()?;
        // SAFETY:
        // - each slot is initialized exactly once during per-CPU scheduler
        //   bring-up before remote scheduling can reference it;
        // - the stored address points at a per-CPU `RunQueue` whose lifetime
        //   is the entire kernel run;
        // - publication through `Once` keeps the pointee live for the kernel;
        // - remote mutation is restricted to the RQ lock and atomic fields.
        Some(unsafe { &*(run_queue as *const RunQueue) })
    }
}

static RUN_QUEUES: RunQueueRegistry = RunQueueRegistry::new();

#[inline(always)]
fn current_run_queue_ptr() -> NonNull<RunQueue> {
    // The guard-free shared accessor is sound because `LazyInit<RunQueue>` is
    // `Sync`; the inner run queue is published during bring-up before normal
    // scheduling paths run.
    let rq = RUN_QUEUE
        .shared_current()
        .get()
        .expect("run queue must be initialized before registration");
    NonNull::from(rq)
}

#[inline(always)]
pub(crate) fn current_run_queue_ref() -> &'static RunQueue {
    // The returned reference is shared; all mutable run-queue state is behind
    // the scheduler lock or atomic fields.
    RUN_QUEUE
        .shared_current()
        .get()
        .expect("run queue must be initialized before use")
}

#[inline(always)]
fn current_wait_for_exit() -> &'static PollSet {
    WAIT_FOR_EXIT
        .shared_current()
        .get()
        .expect("wait-for-exit must be initialized before GC/exit paths")
}

#[inline]
fn with_run_queue<R>(f: impl FnOnce(&mut LazyInit<RunQueue>) -> R) -> R {
    // Bring-up runs on the owning CPU with IRQs off; the guard pins it while
    // the lazy slot is published.
    let guard = kspin::NoPreemptIrqSave::new();
    // SAFETY: the guard pins the owning CPU and this is the only access while
    // the lazy slot is being published.
    f(unsafe { RUN_QUEUE.get_mut_unchecked(&guard) })
}

#[inline]
fn with_idle_task<R>(f: impl FnOnce(&mut LazyInit<KtaskRef>) -> R) -> R {
    // As `with_run_queue`.
    let guard = kspin::NoPreemptIrqSave::new();
    // SAFETY: as `with_run_queue`.
    f(unsafe { IDLE_TASK.get_mut_unchecked(&guard) })
}

#[inline]
fn with_wait_for_exit<R>(f: impl FnOnce(&mut LazyInit<PollSet>) -> R) -> R {
    // As `with_run_queue`; the GC/exit paths run on the owning CPU.
    let guard = kspin::NoPreemptIrqSave::new();
    // SAFETY: as `with_run_queue`.
    f(unsafe { WAIT_FOR_EXIT.get_mut_unchecked(&guard) })
}

#[inline]
fn with_exited_tasks<R>(f: impl FnOnce(&mut VecDeque<KtaskRef>) -> R) -> R {
    // `SpinNoIrq` is `Sync`; object exclusivity comes from its lock.
    f(&mut EXITED_TASKS.shared_current().lock())
}

/// Returns a guard over the current CPU's run queue.
///
/// Callers must run after scheduler bring-up. The `G` acquire/release pair
/// provides the IRQ/preempt exclusion that makes the `'static` borrow sound.
#[inline(always)]
pub(crate) fn current_run_queue<G: BaseGuard>() -> CurrentRunQueueRef<'static, G> {
    let irq_state = G::acquire();
    CurrentRunQueueRef {
        inner: current_run_queue_ref(),
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
fn get_run_queue(index: usize) -> &'static RunQueue {
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
    last_accounted_ns: AtomicU64,
    /// Runnable snapshot: `nr_queued + (curr is a non-idle class)`.
    ///
    /// Same formula as `refactor_ksched` `RunQueue::load_count`. Written
    /// under [`Self::scheduler`] after every policy mutation. Incremental
    /// charge/discharge drifted when idle was `curr` and Fair still had
    /// waiters, so SIS never saw a free CPU.
    #[cfg(feature = "smp")]
    nr_running: AtomicUsize,
    /// Sticky-home occupancy: running, ready, **and** blocked tasks whose
    /// `cpu_id` still names this RQ. Second spawn key after idle-first, so
    /// two idle CPUs prefer the one with fewer sticky residents.
    #[cfg(feature = "smp")]
    nr_home: AtomicUsize,
    /// Snapshot of [`ksched::Scheduler::rt_preempt_rank`] for RT CPU pick.
    ///
    /// Written under [`Self::scheduler`] with `nr_running`. Remote selection
    /// reads it without that lock. A stale rank only mis-places one wakeup.
    #[cfg(feature = "smp")]
    rt_preempt_rank: AtomicU8,
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
    inner: &'a RunQueue,
    state: G::State,
    _phantom: core::marker::PhantomData<G>,
}

/// A reference to the current run queue with specific guard.
///
/// [`CurrentRunQueueRef`] is used to get a reference to the run queue on current CPU,
/// in which scheduling operations can be performed.
pub(crate) struct CurrentRunQueueRef<'a, G: BaseGuard> {
    inner: &'a RunQueue,
    current_task: CurrentTask,
    state: G::State,
    _phantom: core::marker::PhantomData<G>,
}

pub(crate) fn init() {
    // First `publish_task` / `install_running` take `SpinRaw` before any
    // `NoPreemptIrqSave` exists. AArch64 writes `DAIF.I` into SPSR only when
    // dropping from EL2/EL3, so an EL1 entry can arrive with IRQs open.
    // Leave them masked until `kruntime` enables them after handlers exist.
    karch::disable_local_irq();
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
    let idle_task = idle_task.into_arc_kernel(ksched::KernelClass::Idle);
    with_idle_task(|i| {
        i.init_once(idle_task.clone());
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

    with_run_queue(|rq| {
        rq.init_once(RunQueue::new(cpu_id, idle_task));
    });
    RUN_QUEUES.register_current(cpu_id, current_run_queue_ptr());
    current_run_queue_ref().install_running(&main_task);
    #[cfg(feature = "smp")]
    current_run_queue_ref().charge_home(&main_task);
}

pub(crate) fn init_secondary() {
    // Same mask as `init`. Secondary entry also has no `NoPreemptIrqSave`
    // before the first scheduler lock, and this mask stays until
    // `rust_main_secondary` calls `enable_local_irq`.
    karch::disable_local_irq();
    let cpu_id = this_cpu_id();

    // Put the subsequent execution into the `idle` task.
    let idle_task =
        TaskInner::new_current_idle("idle".into()).into_arc_kernel(ksched::KernelClass::Idle);
    idle_task.set_state(TaskState::Running);
    #[cfg(feature = "smp")]
    RunQueue::set_owner_cpu(&idle_task, cpu_id);
    #[cfg(feature = "snapshot")]
    crate::task_registry::record_tracked_task(&idle_task);
    with_idle_task(|i| {
        i.init_once(idle_task.clone());
    });
    // SAFETY: scheduler bring-up installs the first current task for this CPU
    // before any concurrent task access can occur.
    unsafe { CurrentTask::init_current(idle_task.clone()) }

    with_run_queue(|rq| {
        rq.init_once(RunQueue::new(cpu_id, idle_task.clone()));
    });
    RUN_QUEUES.register_current(cpu_id, current_run_queue_ptr());
    current_run_queue_ref().install_running(&idle_task);
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-CPU run queue implementation and scheduling helpers.

#[cfg(feature = "sched_stat")]
use alloc::string::String;
#[cfg(feature = "smp")]
use alloc::sync::Weak;
use alloc::{collections::VecDeque, sync::Arc};
#[cfg(feature = "sched_stat")]
use core::fmt::Write;
#[cfg(feature = "sched_stat")]
use core::sync::atomic::AtomicU64;
#[cfg(feature = "smp")]
use core::sync::atomic::AtomicUsize;
#[cfg(any(feature = "smp", feature = "sched_stat"))]
use core::sync::atomic::Ordering;
use core::{
    future::poll_fn,
    ptr::NonNull,
    task::{Context, Poll},
};

use kcpu_id_map::{KCpuMaskExt, LogicalCpuId};
use khal::percpu::this_cpu_id;
use klazy::Once;
use kpoll::{PollRegistrations, PollSet};
use ksched::{BaseScheduler, CurrentDisposition};
use kspin::{BaseGuard, SpinNoIrqGuard, SpinRaw};
use lazyinit::LazyInit;

#[cfg(all(feature = "preempt", feature = "smp", feature = "ipi"))]
use crate::api::on_remote_resched_ipi;
use crate::{
    KCpuMask, KtaskRef, Scheduler, TaskInner,
    api::{
        current, current_may_uninit, program_sched_deadline, program_sched_deadline_for,
        rearm_local_timer,
    },
    future::{block_on, earliest_deadline},
    task::{CurrentTask, TaskState},
    tracing_hooks::{fire_context_switch, fire_task_wakeup},
};

#[cfg(feature = "sched_stat")]
struct SchedCpuStats {
    select_task: AtomicU64,
    select_wakeup: AtomicU64,
    wakeup_last_cpu: AtomicU64,
    wakeup_idle_sibling: AtomicU64,
    wakeup_fallback: AtomicU64,
    add_task: AtomicU64,
    unblock_task: AtomicU64,
    local_resched: AtomicU64,
    remote_resched: AtomicU64,
    remote_resched_fail: AtomicU64,
    tick_preempt: AtomicU64,
    /// Hardware timer IRQ classified as schedule-deadline due.
    timer_irq_sched: AtomicU64,
    /// Hardware timer IRQ classified as soft-timer due.
    timer_irq_soft: AtomicU64,
    /// Hardware timer IRQ classified as periodic-callback due.
    timer_irq_periodic: AtomicU64,
    /// Hardware timer IRQ with no source due (clamp early / cancel race).
    timer_irq_stale: AtomicU64,
    preempt_check: AtomicU64,
    preempt_need: AtomicU64,
    preempt_skip_disabled: AtomicU64,
    preempt_skip_exception: AtomicU64,
    preempt_resched: AtomicU64,
    preempt_denied: AtomicU64,
    resched: AtomicU64,
    pick_idle: AtomicU64,
    /// Successful idle-pull of a Ready `!on_cpu` waiter from a busier CPU.
    idle_pull: AtomicU64,
    switch: AtomicU64,
    switch_same: AtomicU64,
}

#[cfg(feature = "sched_stat")]
impl SchedCpuStats {
    const fn new() -> Self {
        Self {
            select_task: AtomicU64::new(0),
            select_wakeup: AtomicU64::new(0),
            wakeup_last_cpu: AtomicU64::new(0),
            wakeup_idle_sibling: AtomicU64::new(0),
            wakeup_fallback: AtomicU64::new(0),
            add_task: AtomicU64::new(0),
            unblock_task: AtomicU64::new(0),
            local_resched: AtomicU64::new(0),
            remote_resched: AtomicU64::new(0),
            remote_resched_fail: AtomicU64::new(0),
            tick_preempt: AtomicU64::new(0),
            timer_irq_sched: AtomicU64::new(0),
            timer_irq_soft: AtomicU64::new(0),
            timer_irq_periodic: AtomicU64::new(0),
            timer_irq_stale: AtomicU64::new(0),
            preempt_check: AtomicU64::new(0),
            preempt_need: AtomicU64::new(0),
            preempt_skip_disabled: AtomicU64::new(0),
            preempt_skip_exception: AtomicU64::new(0),
            preempt_resched: AtomicU64::new(0),
            preempt_denied: AtomicU64::new(0),
            resched: AtomicU64::new(0),
            pick_idle: AtomicU64::new(0),
            idle_pull: AtomicU64::new(0),
            switch: AtomicU64::new(0),
            switch_same: AtomicU64::new(0),
        }
    }
}

#[cfg(feature = "sched_stat")]
static SCHED_CPU_STATS: [SchedCpuStats; kbuild_config::NR_CPUS] =
    [const { SchedCpuStats::new() }; kbuild_config::NR_CPUS];

#[cfg(feature = "sched_stat")]
#[inline]
fn sched_stat_cpu(cpu_id: LogicalCpuId) -> &'static SchedCpuStats {
    &SCHED_CPU_STATS[cpu_id.as_usize()]
}

#[cfg(feature = "sched_stat")]
#[inline]
fn sched_stat_inc(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

#[cfg(feature = "sched_stat")]
pub(crate) fn dump_sched_stats() {
    // Keep the watchdog path allocation-free and independent of logger locks.
    khal::kprint_atomic!("[sched_stat] begin\n");
    for (cpu, stats) in SCHED_CPU_STATS.iter().enumerate() {
        khal::kprint_atomic!(
            "[sched_stat] cpu={} select_task={} select_wakeup={} wakeup_last_cpu={} \
             wakeup_idle_sibling={} wakeup_fallback={} add_task={} unblock={} local_resched={} \
             remote_resched={} remote_resched_fail={} tick_preempt={} timer_irq_sched={} \
             timer_irq_soft={} timer_irq_periodic={} timer_irq_stale={} preempt_check={} \
             preempt_need={} preempt_skip_disabled={} preempt_skip_exception={} \
             preempt_resched={} preempt_denied={} resched={} pick_idle={} idle_pull={} switch={} \
             switch_same={}\n",
            cpu,
            stats.select_task.load(Ordering::Relaxed),
            stats.select_wakeup.load(Ordering::Relaxed),
            stats.wakeup_last_cpu.load(Ordering::Relaxed),
            stats.wakeup_idle_sibling.load(Ordering::Relaxed),
            stats.wakeup_fallback.load(Ordering::Relaxed),
            stats.add_task.load(Ordering::Relaxed),
            stats.unblock_task.load(Ordering::Relaxed),
            stats.local_resched.load(Ordering::Relaxed),
            stats.remote_resched.load(Ordering::Relaxed),
            stats.remote_resched_fail.load(Ordering::Relaxed),
            stats.tick_preempt.load(Ordering::Relaxed),
            stats.timer_irq_sched.load(Ordering::Relaxed),
            stats.timer_irq_soft.load(Ordering::Relaxed),
            stats.timer_irq_periodic.load(Ordering::Relaxed),
            stats.timer_irq_stale.load(Ordering::Relaxed),
            stats.preempt_check.load(Ordering::Relaxed),
            stats.preempt_need.load(Ordering::Relaxed),
            stats.preempt_skip_disabled.load(Ordering::Relaxed),
            stats.preempt_skip_exception.load(Ordering::Relaxed),
            stats.preempt_resched.load(Ordering::Relaxed),
            stats.preempt_denied.load(Ordering::Relaxed),
            stats.resched.load(Ordering::Relaxed),
            stats.pick_idle.load(Ordering::Relaxed),
            stats.idle_pull.load(Ordering::Relaxed),
            stats.switch.load(Ordering::Relaxed),
            stats.switch_same.load(Ordering::Relaxed),
        );
    }
    khal::kprint_atomic!("[sched_stat] end\n");
}

/// Classifies a local hardware timer IRQ against the known deadline sources.
#[cfg(feature = "sched_stat")]
pub(crate) fn note_timer_irq(sched_due: bool, soft_due: bool, periodic_due: bool) {
    let stats = sched_stat_cpu(this_cpu_id());
    if !(sched_due || soft_due || periodic_due) {
        sched_stat_inc(&stats.timer_irq_stale);
        return;
    }
    if sched_due {
        sched_stat_inc(&stats.timer_irq_sched);
    }
    if soft_due {
        sched_stat_inc(&stats.timer_irq_soft);
    }
    if periodic_due {
        sched_stat_inc(&stats.timer_irq_periodic);
    }
}

#[cfg(feature = "sched_stat")]
pub(crate) fn sched_stats_text() -> String {
    let mut out = String::new();
    let _ = writeln!(out, "[sched_stat] begin");
    for (cpu, stats) in SCHED_CPU_STATS.iter().enumerate() {
        let _ = writeln!(
            out,
            "[sched_stat] cpu={} select_task={} select_wakeup={} wakeup_last_cpu={} \
             wakeup_idle_sibling={} wakeup_fallback={} add_task={} unblock={} local_resched={} \
             remote_resched={} remote_resched_fail={} tick_preempt={} timer_irq_sched={} \
             timer_irq_soft={} timer_irq_periodic={} timer_irq_stale={} preempt_check={} \
             preempt_need={} preempt_skip_disabled={} preempt_skip_exception={} \
             preempt_resched={} preempt_denied={} resched={} pick_idle={} idle_pull={} switch={} \
             switch_same={}",
            cpu,
            stats.select_task.load(Ordering::Relaxed),
            stats.select_wakeup.load(Ordering::Relaxed),
            stats.wakeup_last_cpu.load(Ordering::Relaxed),
            stats.wakeup_idle_sibling.load(Ordering::Relaxed),
            stats.wakeup_fallback.load(Ordering::Relaxed),
            stats.add_task.load(Ordering::Relaxed),
            stats.unblock_task.load(Ordering::Relaxed),
            stats.local_resched.load(Ordering::Relaxed),
            stats.remote_resched.load(Ordering::Relaxed),
            stats.remote_resched_fail.load(Ordering::Relaxed),
            stats.tick_preempt.load(Ordering::Relaxed),
            stats.timer_irq_sched.load(Ordering::Relaxed),
            stats.timer_irq_soft.load(Ordering::Relaxed),
            stats.timer_irq_periodic.load(Ordering::Relaxed),
            stats.timer_irq_stale.load(Ordering::Relaxed),
            stats.preempt_check.load(Ordering::Relaxed),
            stats.preempt_need.load(Ordering::Relaxed),
            stats.preempt_skip_disabled.load(Ordering::Relaxed),
            stats.preempt_skip_exception.load(Ordering::Relaxed),
            stats.preempt_resched.load(Ordering::Relaxed),
            stats.preempt_denied.load(Ordering::Relaxed),
            stats.resched.load(Ordering::Relaxed),
            stats.pick_idle.load(Ordering::Relaxed),
            stats.idle_pull.load(Ordering::Relaxed),
            stats.switch.load(Ordering::Relaxed),
            stats.switch_same.load(Ordering::Relaxed),
        );
    }

    #[cfg(all(feature = "sched_eevdf", feature = "smp"))]
    {
        // Per-CPU guard + skip unregistered RQs: `/proc/sched_stat` must not
        // hold IRQ/preempt off across all CPUs, nor panic on a present-but-not-
        // brought-up CPU.
        for cpu in 0..kcpu_id_map::nr_cpus() {
            let stats = {
                let _guard = kspin::NoPreemptIrqSave::new();
                let Some(rq) = RUN_QUEUES.try_get(cpu) else {
                    continue;
                };
                rq.scheduler.lock().stats()
            };
            let _ = writeln!(
                out,
                "[eevdf_stat] cpu={} picks={} deadline_preempt={} fallback_no_eligible={} \
                 slice_expired={} wake_handoff={} wake_handoff_skipped_busy={} \
                 wake_sync_preempt={}",
                cpu,
                stats.picks_total,
                stats.preempt_by_deadline,
                stats.fallback_no_eligible,
                stats.slice_expired,
                stats.wake_handoff,
                stats.wake_handoff_skipped_busy,
                stats.wake_sync_preempt,
            );
            let _ = writeln!(
                out,
                "[eevdf_wake] cpu={} mark={} mark_no_buddy={} nom_no_curr={} probe_no_buddy={} \
                 probe_ineligible={} probe_false_buddy={} buddy_drop={}",
                cpu,
                stats.wake_sync_mark,
                stats.wake_sync_mark_no_buddy,
                stats.wake_nominate_no_curr,
                stats.probe_sync_no_buddy,
                stats.probe_sync_ineligible,
                stats.probe_false_with_buddy,
                stats.buddy_pick_drop,
            );
        }
    }

    let _ = writeln!(out, "[sched_stat] end");
    out
}

#[cfg(not(feature = "sched_stat"))]
pub(crate) fn dump_sched_stats() {}

#[cfg(not(feature = "sched_stat"))]
pub(crate) fn sched_stats_text() -> alloc::string::String {
    "scheduler statistics disabled\n".into()
}

#[cfg(feature = "sched_stat")]
#[inline]
pub(crate) fn record_preempt_pending_check(need_resched: bool) {
    let stats = sched_stat_cpu(this_cpu_id());
    sched_stat_inc(&stats.preempt_check);
    if need_resched {
        sched_stat_inc(&stats.preempt_need);
    }
}

#[cfg(not(feature = "sched_stat"))]
#[inline]
pub(crate) fn record_preempt_pending_check(_need_resched: bool) {}

#[cfg(feature = "sched_stat")]
#[inline]
pub(crate) fn record_preempt_pending_blocked(can_preempt: bool, in_exception: bool) {
    let stats = sched_stat_cpu(this_cpu_id());
    if !can_preempt {
        sched_stat_inc(&stats.preempt_skip_disabled);
    }
    if in_exception {
        sched_stat_inc(&stats.preempt_skip_exception);
    }
}

#[cfg(not(feature = "sched_stat"))]
#[inline]
pub(crate) fn record_preempt_pending_blocked(_can_preempt: bool, _in_exception: bool) {}

#[cfg(all(
    feature = "preempt",
    feature = "smp",
    feature = "ipi",
    feature = "sched_stat"
))]
#[inline]
fn record_remote_resched(cpu_id: LogicalCpuId, failed: bool) {
    let stats = sched_stat_cpu(cpu_id);
    sched_stat_inc(&stats.remote_resched);
    if failed {
        sched_stat_inc(&stats.remote_resched_fail);
    }
}

#[cfg(all(
    feature = "preempt",
    feature = "smp",
    feature = "ipi",
    not(feature = "sched_stat")
))]
#[inline]
fn record_remote_resched(_cpu_id: LogicalCpuId, _failed: bool) {}

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
        current_task: current(),
        state: irq_state,
        _phantom: core::marker::PhantomData,
    }
}

/// Fork / migrate-in CPU pick, analogue of Linux `find_idlest_cpu`.
///
/// Linux fork is idle-first (`idle_cpu()`, true even with sleepers) then PELT
/// `load_avg`. Wake is already `select_idle_sibling`, so landing a clone on a
/// sleeper CPU is safe: the displaced worker can seek idle on the next wake.
/// Occupancy among idle (and among busy) CPUs is still [`RunQueue::nr_home`].
/// Ties: `prefer_local`, then RR.
///
/// Wake idle-seek uses [`select_idle_cpu`], not this ranking. Home-forbidden
/// and no-idle wake fallback calls this with `prefer_local == false`.
///
/// ## Panics
///
/// Empty `cpumask`, or none of the allowed CPUs have finished scheduler
/// bring-up.
#[cfg(feature = "smp")]
#[inline]
fn select_run_queue_index(cpumask: &KCpuMask, prefer_local: bool) -> usize {
    static RUN_QUEUE_INDEX: AtomicUsize = AtomicUsize::new(0);

    let rr_token = RUN_QUEUE_INDEX.fetch_add(1, Ordering::Relaxed);
    let prefer = prefer_local.then(|| this_cpu_id().as_usize());
    let index = find_idlest_cpu(cpumask, rr_token, prefer, |idx| {
        RUN_QUEUES.try_get(idx).as_deref().map(spawn_idlest_key)
    });
    #[cfg(feature = "sched_stat")]
    sched_stat_inc(&sched_stat_cpu(LogicalCpuId::new(index)).select_task);
    index
}

/// Spawn key: lower is better. `(is_busy, nr_home)`.
///
/// Idle (`nr_running == 0`) ranks ahead of any busy CPU, matching Linux
/// `idle_cpu()` first. Among idle (or among busy), lower [`RunQueue::nr_home`]
/// wins so a vacant CPU beats a sleeper's CPU.
#[cfg(feature = "smp")]
fn spawn_idlest_key(rq: &RunQueue) -> (bool, usize) {
    let nr_home = rq.nr_home.load(Ordering::Relaxed);
    let nr_running = rq.nr_running.load(Ordering::Relaxed);
    let is_busy = nr_running > 0;
    (is_busy, nr_home)
}

/// Lowest `key_of` in `cpumask`; `prefer` wins remaining ties, else `rr_token`.
///
/// `key_of` returns [`None`] for a CPU that is not yet schedulable (skipped).
#[cfg(feature = "smp")]
fn find_idlest_cpu<K: Copy + Ord>(
    cpumask: &KCpuMask,
    rr_token: usize,
    prefer: Option<usize>,
    key_of: impl Fn(usize) -> Option<K>,
) -> usize {
    assert!(!cpumask.is_empty(), "No available CPU for task execution");

    let mut tied = [0usize; kbuild_config::NR_CPUS];
    let mut n_tied = 0usize;
    let mut best_key: Option<K> = None;
    let mut is_prefer_tied = false;
    for cpu_id in cpumask.iter_logical() {
        let idx = cpu_id.as_usize();
        let Some(key) = key_of(idx) else {
            continue;
        };
        let is_better = best_key.is_none_or(|best| key < best);
        if is_better {
            best_key = Some(key);
            tied[0] = idx;
            n_tied = 1;
            is_prefer_tied = prefer == Some(idx);
        } else if best_key == Some(key) {
            tied[n_tied] = idx;
            n_tied += 1;
            if prefer == Some(idx) {
                is_prefer_tied = true;
            }
        }
    }
    assert!(n_tied > 0, "No available CPU for task execution");
    if n_tied == 1 {
        tied[0]
    } else if let Some(pref) = prefer.filter(|_| is_prefer_tied) {
        pref
    } else {
        tied[rr_token % n_tied]
    }
}

/// Busiest CPU with `nr_running >= 2`, excluding `dest` and `tried`.
///
/// Linux `idle_balance` pulls onto a CPU that is about to idle. We only steal
/// from a CPU that still has a waiter after keeping its runner
/// (`nr_running >= 2`). Ties keep the lowest CPU id.
#[cfg(feature = "smp")]
fn find_idle_pull_src(
    dest: usize,
    tried: &KCpuMask,
    nr_cpus: usize,
    nr_running_of: impl Fn(usize) -> usize,
) -> Option<usize> {
    let mut best_cpu = None;
    let mut best_nr = 1usize;
    for cpu in 0..nr_cpus {
        if cpu == dest || tried.get(cpu) {
            continue;
        }
        let nr_running = nr_running_of(cpu);
        if nr_running > best_nr {
            best_nr = nr_running;
            best_cpu = Some(cpu);
        }
    }
    best_cpu
}

/// Idle-pull may take this ready waiter onto `dest_cpu`.
///
/// Rejects still-`on_cpu` tasks: yield/preempt requeue before `switch_to`
/// finishes, and stealing that stack races the outgoing CPU.
#[cfg(feature = "smp")]
fn can_idle_pull_task(task: &KtaskRef, dest_cpu: usize) -> bool {
    task.is_ready() && !task.on_cpu() && !task.is_idle() && task.cpumask().get(dest_cpu)
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
/// * In a multi-core system, this function selects the run queue based on the
///   task's CPU affinity and spawn idlest ranking (idle-first, then `nr_home`,
///   then the creating CPU).
///
/// ## Arguments
///
/// * `task` - A reference to the task for which a run queue is being selected.
///
/// ## Returns
///
/// * [`KRunQueueRef`] - a static reference to the selected [`RunQueue`] (current or remote).
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
        // Spawn / migrate-in: find_idlest (not wake select_idle_sibling).
        let index = select_run_queue_index(&task.cpumask(), true);
        KRunQueueRef {
            inner: get_run_queue(index),
            state: irq_state,
            _phantom: core::marker::PhantomData,
        }
    }
}

/// How [`wake_affinity_select`] placed a wakeup.
#[cfg(feature = "smp")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WakePlacement {
    /// `prev_cpu` was idle, or no idle CPU existed so Linux keeps prev.
    PrevCpu,
    /// Home was busy; picked another `nr_running == 0` CPU.
    IdleSibling,
    /// Home not in `cpumask` and no idle CPU; spawn `find_idlest_cpu`.
    Fallback,
}

/// Linux `select_idle_sibling`: `prev` if idle, else the first idle CPU in `cpumask`.
///
/// `is_idle` is true when that CPU is schedulable and has `nr_running == 0`.
/// [`None`] means no idle CPU; the caller keeps `prev` when it is still allowed.
#[cfg(feature = "smp")]
fn select_idle_cpu(
    cpumask: &KCpuMask,
    prev: Option<usize>,
    is_idle: impl Fn(usize) -> bool,
) -> Option<usize> {
    if let Some(prev) = prev
        && cpumask.get(prev)
        && is_idle(prev)
    {
        return Some(prev);
    }
    for cpu_id in cpumask.iter_logical() {
        let idx = cpu_id.as_usize();
        if is_idle(idx) {
            return Some(idx);
        }
    }
    None
}

#[cfg(feature = "smp")]
fn is_runqueue_idle(idx: usize) -> bool {
    RUN_QUEUES
        .try_get(idx)
        .is_some_and(|rq| rq.nr_running.load(Ordering::Relaxed) == 0)
}

/// Wake placement analogue of Linux `select_idle_sibling`.
///
/// Sticky-home plus "home busy → idle" overflow collapsed schbench to ~3-way
/// parallelism (RPS ~450 → ~325) because the overflowed task never left.
/// Linux keeps prev when it is idle, otherwise seeks an idle CPU; the next
/// wake of the displaced worker can seek again (musical chairs, still ~4-way).
///
/// No idle CPU: stay on prev if allowed. Home forbidden: spawn `find_idlest`.
/// Exposed to unittest so the branch is covered without `/proc/sched_stat`.
#[cfg(feature = "smp")]
#[inline]
pub(crate) fn wake_affinity_select(task: &KtaskRef) -> (usize, WakePlacement) {
    let cpumask = task.cpumask();
    let home = task.cpu_id().as_usize();
    let is_home_allowed = cpumask.get(home);
    let prev = is_home_allowed.then_some(home);

    if let Some(cpu) = select_idle_cpu(&cpumask, prev, is_runqueue_idle) {
        let place = if prev == Some(cpu) {
            WakePlacement::PrevCpu
        } else {
            WakePlacement::IdleSibling
        };
        return (cpu, place);
    }
    if is_home_allowed {
        (home, WakePlacement::PrevCpu)
    } else {
        (
            select_run_queue_index(&cpumask, false),
            WakePlacement::Fallback,
        )
    }
}

/// Selects a run queue for a task that is becoming runnable from a wakeup.
///
/// Linux `select_idle_sibling`: prev if idle, else an idle CPU in `cpumask`.
/// See [`wake_affinity_select`].
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
        let (index, place) = wake_affinity_select(task);
        #[cfg(feature = "sched_stat")]
        {
            let stats = sched_stat_cpu(LogicalCpuId::new(index));
            sched_stat_inc(&stats.select_wakeup);
            match place {
                WakePlacement::PrevCpu => sched_stat_inc(&stats.wakeup_last_cpu),
                WakePlacement::IdleSibling => sched_stat_inc(&stats.wakeup_idle_sibling),
                WakePlacement::Fallback => sched_stat_inc(&stats.wakeup_fallback),
            }
        }
        #[cfg(not(feature = "sched_stat"))]
        let _ = place;
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
fn request_resched_on(rq: &mut RunQueue) {
    let cpu_id = rq.cpu_id;
    if cpu_id == this_cpu_id() {
        #[cfg(feature = "sched_stat")]
        sched_stat_inc(&sched_stat_cpu(cpu_id).local_resched);
        current().set_preempt_pending(true);
        // A newly runnable peer may require an earlier schedule deadline even
        // when the current task previously needed no timer (lone task).
        let now_ns = khal::time::monotonic_time_nanos();
        let curr = current();
        let next_rel = if curr.is_idle() {
            None
        } else {
            let curr_arc = curr.clone();
            rq.scheduler.lock().next_preemption_ns(&curr_arc)
        };
        program_sched_deadline(now_ns, next_rel);
        rearm_local_timer(earliest_deadline(cpu_id), None);
    } else {
        #[cfg(all(feature = "smp", feature = "ipi"))]
        {
            let resched_result = request_remote_resched(cpu_id);
            record_remote_resched(cpu_id, resched_result.is_err());
        }
    }
}

#[cfg(all(feature = "preempt", feature = "smp", feature = "ipi"))]
fn request_remote_resched(cpu_id: LogicalCpuId) -> kipi::Result<()> {
    if let Err(err) = kipi::run_on_cpu(cpu_id, || {
        on_remote_resched_ipi();
    }) {
        warn!(
            "failed to request reschedule on CPU {}: {}",
            cpu_id.as_usize(),
            err
        );
        Err(err)
    } else {
        Ok(())
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
        #[cfg(feature = "sched_stat")]
        sched_stat_inc(&sched_stat_cpu(self.inner.cpu_id).add_task);
        // Flush wall time into the running entity before placement so a NOHZ
        // lone task cannot leave system V stale across add/wake.
        let now_ns = khal::time::monotonic_time_nanos();
        self.inner.flush_running_runtime(now_ns);
        self.inner.publish_task(task);

        // Dynamic schedule timers are disarmed while a task runs alone, so
        // every newly published peer must refresh the target CPU. This is also
        // required for a busy local CPU: there is no periodic tick to discover
        // the new ready entity later.
        #[cfg(feature = "preempt")]
        request_resched_on(self.inner);
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
        // Account + PLACE + WF_SYNC share one scheduler lock so a remote CPU
        // cannot leave/pick between flush and nominate (HEAD had no flush).
        let is_wake_sync = current_may_uninit().is_some_and(|c| c.is_wake_sync());
        let wake_enqueue =
            self.inner
                .put_task_with_state(task, TaskState::Blocked, is_wake_sync, resched);
        if wake_enqueue != WakeEnqueue::Rejected {
            // Since now, the task to be unblocked is in the `Ready` state.
            let cpu_id = self.inner.cpu_id;
            debug!(
                "task unblock: {task_id_name} on run_queue {}",
                cpu_id.as_usize()
            );
            #[cfg(feature = "sched_stat")]
            sched_stat_inc(&sched_stat_cpu(cpu_id).unblock_task);

            // Fire the task wakeup tracepoint.
            fire_task_wakeup(task_id);

            #[cfg(feature = "preempt")]
            if resched && wake_enqueue == WakeEnqueue::Enqueued {
                request_resched_on(self.inner);
            }
        }
    }
}

/// Core functions of run queue.
impl<G: BaseGuard> CurrentRunQueueRef<'_, G> {
    /// Accounts wall-clock runtime since the last account point for `current`.
    ///
    /// Matches Linux `update_curr`: request-done may set `need_resched`. Peer
    /// pick comparison belongs on the schedule-tick account path.
    pub fn account_current_runtime(&mut self, now_ns: u64) {
        self.account_runtime(now_ns, false);
    }

    /// Linux `entity_tick`: account, then EEVDF `check_preempt_tick`.
    ///
    /// Only the due schedule slot uses this. Soft/periodic IRQs must not
    /// re-evaluate pick — that ping-ponged WF_SYNC later-deadline handoffs.
    #[cfg(feature = "sched_eevdf")]
    pub fn account_sched_tick(&mut self, now_ns: u64) {
        self.account_runtime(now_ns, true);
    }

    fn account_runtime(&mut self, now_ns: u64, is_sched_tick: bool) {
        let curr = &self.current_task;
        let is_idle = curr.is_idle();
        let (elapsed, should_preempt) = {
            let mut sched = self.inner.scheduler.lock();
            let elapsed =
                RunQueue::advance_account_epoch(&mut self.inner.last_accounted_ns, now_ns);
            if is_idle {
                (elapsed, false)
            } else {
                let curr_arc = curr.clone();
                let mut should_preempt = elapsed > 0 && sched.update_current(&curr_arc, elapsed);
                #[cfg(feature = "sched_eevdf")]
                if is_sched_tick && !should_preempt {
                    should_preempt = sched.check_preempt_tick();
                }
                #[cfg(not(feature = "sched_eevdf"))]
                let _ = is_sched_tick;
                (elapsed, should_preempt)
            }
        };
        self.apply_runtime_account_side_effects(elapsed, is_idle, should_preempt);
    }

    fn apply_runtime_account_side_effects(
        &mut self,
        elapsed: u64,
        is_idle: bool,
        should_preempt: bool,
    ) {
        if is_idle {
            return;
        }
        if elapsed > 0
            && let Some(worker_context) = kwork::raw::WorkqueueTaskContextIf::current_work_context()
        {
            let wake_plan = worker_context.worker_tick_in_scheduler();
            self.inner
                .execute_system_worker_wake_plan(worker_context, wake_plan);
        }
        #[cfg(feature = "sched_stat")]
        if should_preempt && elapsed > 0 {
            sched_stat_inc(&sched_stat_cpu(self.inner.cpu_id).tick_preempt);
        }
        #[cfg(not(feature = "sched_stat"))]
        let _ = elapsed;
        if should_preempt {
            #[cfg(feature = "preempt")]
            self.current_task.set_preempt_pending(true);
        }
    }

    /// Recomputes the per-CPU schedule deadline for the running task.
    pub fn refresh_sched_deadline(&mut self, now_ns: u64) {
        let curr = &self.current_task;
        let next_rel = if curr.is_idle() {
            None
        } else {
            let curr_arc = curr.clone();
            self.inner.scheduler.lock().next_preemption_ns(&curr_arc)
        };
        program_sched_deadline(now_ns, next_rel);
    }

    /// Yield the current task and reschedule.
    /// This function will put the current task into this run queue with `Ready` state,
    /// and reschedule to the next task on this run queue.
    pub fn yield_current(&mut self) {
        let now_ns = khal::time::monotonic_time_nanos();
        self.account_current_runtime(now_ns);
        let curr = &self.current_task;
        trace!("task yield: {}", curr.id_name());
        assert!(curr.is_running());

        // Affinity may have been tightened by another thread; leave this CPU
        // instead of requeueing onto a forbidden run queue.
        #[cfg(feature = "smp")]
        if !curr.cpumask().get(this_cpu_id().as_usize()) {
            let migration_task = spawn_affinity_migration_task(curr.clone());
            self.migrate_current(migration_task);
            return;
        }

        assert!(
            curr.transition_state(TaskState::Running, TaskState::Ready),
            "yield_current requires Running -> Ready before leave_current"
        );
        // Idle is never tracked by the algorithm; only flip Ready so `resched`
        // can re-select it (or another task) without enqueueing into the RQ.
        // EEVDF `curr` stays unset for idle (see `switch_to_local` sync).
        if !curr.is_idle() {
            self.inner
                .scheduler
                .lock()
                .leave_current(curr.clone(), CurrentDisposition::Yield);
        }
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
        let now_ns = khal::time::monotonic_time_nanos();
        self.account_current_runtime(now_ns);
        let curr = &self.current_task;
        trace!("task migrate: {}", curr.id_name());
        assert!(curr.is_running());

        // Migration helper already owns a strong ref (spawned with curr.clone()).
        // Deactivate on the source RQ before departure; destination `enqueue_task`
        // then applies PLACE_LAG via `needs_place`.
        self.inner
            .scheduler
            .lock()
            .leave_current(curr.clone(), CurrentDisposition::Migrate);
        #[cfg(feature = "smp")]
        self.inner.discharge_rq_load(curr);

        // Mark current task's state as `Ready`,
        // but, do not put current task to the scheduler of this run queue.
        curr.set_state(TaskState::Ready);

        // Migration helper is switched to without enqueue; attach ownership first.
        self.inner.switch_to_local(current(), migration_task);
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
        let now_ns = khal::time::monotonic_time_nanos();
        assert!(self.current_task.is_running());

        // When we call `preempt_resched()`, both IRQs and preemption must
        // have been disabled by `kspin::NoPreemptIrqSave`. So we need
        // to set `current_disable_count` to 1 in `can_preempt()` to obtain
        // the preemption permission.
        let can_preempt = self.current_task.can_preempt(1);
        #[cfg(feature = "sched_stat")]
        {
            let stats = sched_stat_cpu(self.inner.cpu_id);
            sched_stat_inc(&stats.preempt_resched);
            if !can_preempt {
                sched_stat_inc(&stats.preempt_denied);
            }
        }

        trace!(
            "current task is to be preempted: {}, allow={}",
            self.current_task.id_name(),
            can_preempt
        );

        #[cfg(feature = "sched_eevdf")]
        let affinity_ok = {
            #[cfg(feature = "smp")]
            {
                self.current_task.cpumask().get(this_cpu_id().as_usize())
            }
            #[cfg(not(feature = "smp"))]
            {
                true
            }
        };

        #[cfg(feature = "sched_eevdf")]
        if can_preempt && affinity_ok {
            // One scheduler lock: account, EEVDF peer probe, and — when the
            // probe fails — next_preemption_ns. Re-arm hardware after release.
            let curr_arc = self.current_task.clone();
            let is_idle = self.current_task.is_idle();
            let (elapsed, should_preempt, peer_wins, next_rel) = self
                .inner
                .account_peer_probe_and_next_deadline(now_ns, &curr_arc, is_idle);
            self.apply_runtime_account_side_effects(elapsed, is_idle, should_preempt);
            if !peer_wins {
                self.current_task.set_preempt_pending(false);
                program_sched_deadline(now_ns, next_rel);
                rearm_local_timer(earliest_deadline(self.inner.cpu_id), None);
                return;
            }
        } else {
            self.account_current_runtime(now_ns);
        }

        #[cfg(not(feature = "sched_eevdf"))]
        self.account_current_runtime(now_ns);

        let curr = &self.current_task;
        if can_preempt {
            // Affinity violations always migrate, even when no ready peer would
            // win an ordinary EEVDF probe (Linux `__set_cpus_allowed_ptr`).
            #[cfg(feature = "smp")]
            if !curr.cpumask().get(this_cpu_id().as_usize()) {
                let migration_task = spawn_affinity_migration_task(curr.clone());
                self.migrate_current(migration_task);
                return;
            }
            assert!(
                curr.transition_state(TaskState::Running, TaskState::Ready),
                "preempt_resched requires Running -> Ready before leave_current"
            );
            if !curr.is_idle() {
                // The into_raw current slot already counts as one strong ref.
                // leave_current(Preempt) requeues a second Arc so the task stays
                // alive across switch_to's current-pointer handoff.
                self.inner
                    .scheduler
                    .lock()
                    .leave_current(curr.clone(), CurrentDisposition::Preempt);
            }
            self.inner.resched();
        } else {
            curr.set_preempt_pending(true);
        }
    }

    /// Exit the current task with the specified exit code.
    /// This function will never return.
    pub fn exit_current(&mut self, exit_code: i32) -> ! {
        let now_ns = khal::time::monotonic_time_nanos();
        self.account_current_runtime(now_ns);
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
            khal::power::power_off();
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

            // Exited list owns the task; `leave_current(Exit)` clears scheduler
            // current accounting (e.g. EEVDF `curr`) and must not arm PLACE_LAG.
            self.inner
                .scheduler
                .lock()
                .leave_current(curr.clone(), CurrentDisposition::Exit);
            #[cfg(feature = "smp")]
            self.inner.discharge_rq_load(curr);
            #[cfg(feature = "smp")]
            self.inner.discharge_home(curr);

            // Schedule to next task.
            self.inner.resched();
        }
        unreachable!("task exited!");
    }

    /// Block the current task and reschedule.
    ///
    /// Marks the current task `Blocked` and leaves the scheduler without requeue
    /// ([`CurrentDisposition::Block`]).
    ///
    /// Workerqueue callbacks are accounted here, not in higher-level wait
    /// wrappers: this is the scheduler's Running→Blocked convergence point and
    /// therefore the X-Kernel counterpart of Linux `wq_worker_sleeping()`.
    /// Callers must not add a second workerqueue sleep/resume wrapper around
    /// this function.
    ///
    /// # Caller obligations
    ///
    /// 1. The caller must hold the wait-queue / waker lock represented by `woke`.
    /// 2. The current task must be running and must not be idle.
    /// 3. The caller must keep an **extra** [`KtaskRef`] to the current task across
    ///    the context switch. The into_raw current slot contributes one strong
    ///    ref; the caller's clone (e.g. from [`crate::future::block_on`]) is the
    ///    second. Together they satisfy `switch_to`'s `strong_count > 1` check.
    ///    `leave_current(Block)` does not requeue, so that caller-owned clone is
    ///    what keeps the task alive until the wait queue / waker takes over.
    ///    Do not drop the clone thinking the current slot "does not count".
    /// 4. The wait-queue lock is released inside this function before rescheduling.
    ///
    /// # Panics
    ///
    /// Panics if `Arc::strong_count(current) <= 1` (missing caller-owned ref).
    /// This is a hard scheduler invariant: there is no recovery path. New
    /// wait-queue / timer callers must clone current before entry — see the
    /// `blocked_resched_survives_with_caller_owned_ref` unittest for a
    /// `block_on`-free template. `#[track_caller]` points the panic at the
    /// call site.
    #[track_caller]
    pub fn blocked_resched(&mut self, mut woke: SpinNoIrqGuard<'_, bool>) {
        let now_ns = khal::time::monotonic_time_nanos();
        self.account_current_runtime(now_ns);
        let curr = &self.current_task;
        assert!(curr.is_running());
        assert!(!curr.is_idle());
        // we must not block current task with preemption disabled.
        // Current expected preempt count is 2 for `NoPreemptIrqSave` because we also hold
        // the `woke` SpinNoIrqGuard lock here.
        #[cfg(feature = "preempt")]
        assert!(curr.can_preempt(2));
        // Current slot (into_raw) = 1; caller's clone (e.g. block_on) = 2nd.
        let strong = Arc::strong_count(curr);
        assert!(
            strong > 1,
            "blocked_resched: strong_count={strong} (need > 1); into_raw current slot counts as 1 \
             — caller must hold another KtaskRef across switch (see block_on / blocked_resched \
             rustdoc)"
        );

        self.inner.account_current_worker_sleeping(curr);

        // Deactivate while still Running / current, before any concurrent wake
        // can requeue us. Fair schedulers snapshot lag for later enqueue PLACE_LAG.
        self.inner
            .scheduler
            .lock()
            .leave_current(curr.clone(), CurrentDisposition::Block);
        #[cfg(feature = "smp")]
        self.inner.discharge_rq_load(curr);

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
        let now_ns = khal::time::monotonic_time_nanos();
        self.account_current_runtime(now_ns);
        let ok = self
            .inner
            .scheduler
            .lock()
            .set_priority(&self.current_task, prio);
        if ok {
            self.refresh_sched_deadline(now_ns);
            rearm_local_timer(earliest_deadline(self.inner.cpu_id), None);
        }
        ok
    }
}

impl<G: BaseGuard> KRunQueueRef<'_, G> {
    pub fn set_task_priority(&mut self, task: &KtaskRef, prio: isize) -> bool {
        let now_ns = khal::time::monotonic_time_nanos();
        self.inner.flush_running_runtime(now_ns);
        let ok = self.inner.scheduler.lock().set_priority(task, prio);
        #[cfg(feature = "preempt")]
        if ok {
            // Weight/deadline changes must re-arbitrate the target CPU timer;
            // otherwise NOHZ keeps waiting on a stale schedule deadline.
            request_resched_on(self.inner);
        }
        ok
    }
}

impl RunQueue {
    /// Create a new run queue for the specified CPU.
    /// The run queue is initialized with a per-CPU gc task in its scheduler.
    fn new(cpu_id: LogicalCpuId) -> Self {
        let gc_task = TaskInner::new_internal(
            || {
                let mut registrations = PollRegistrations::new();
                block_on(poll_fn(move |cx| poll_gc(cx, &mut registrations)))
            },
            "gc".into(),
            kbuild_config::TASK_STACK_SIZE,
        )
        .into_arc();
        // gc task should be pinned to the current CPU.
        gc_task.set_cpumask(KCpuMask::one_shot_logical(cpu_id));

        WAIT_FOR_EXIT.with_current(|wait_for_exit| {
            wait_for_exit.init_once(PollSet::new());
        });

        let scheduler = Scheduler::new();
        #[cfg(all(feature = "sched_stat", feature = "sched_eevdf"))]
        let scheduler = {
            let mut scheduler = scheduler;
            scheduler.set_stats_enabled(true);
            scheduler
        };
        let mut rq = Self {
            cpu_id,
            scheduler: SpinRaw::new(scheduler),
            last_accounted_ns: 0,
            #[cfg(feature = "smp")]
            nr_running: AtomicUsize::new(0),
            #[cfg(feature = "smp")]
            nr_home: AtomicUsize::new(0),
        };
        // Publish through the ownership-aware entry so secondary-CPU gc tasks
        // do not keep the default `cpu_id == 0`.
        rq.publish_task(gc_task);
        rq
    }

    /// Account, EEVDF peer-preempt probe, and backup deadline under one lock.
    ///
    /// Returns `(elapsed, should_preempt, peer_wins, next_rel_on_probe_fail)`.
    #[cfg(all(feature = "preempt", feature = "sched_eevdf"))]
    fn account_peer_probe_and_next_deadline(
        &mut self,
        now_ns: u64,
        curr: &KtaskRef,
        is_idle: bool,
    ) -> (u64, bool, bool, Option<u64>) {
        let mut sched = self.scheduler.lock();
        let elapsed = Self::advance_account_epoch(&mut self.last_accounted_ns, now_ns);
        let should_preempt = if is_idle || elapsed == 0 {
            false
        } else {
            sched.update_current(curr, elapsed)
        };
        let peer_wins = sched.peer_preempts_curr();
        let next_rel = if !peer_wins && !is_idle {
            sched.next_preemption_ns(curr)
        } else {
            None
        };
        (elapsed, should_preempt, peer_wins, next_rel)
    }

    /// Advances the account epoch and returns elapsed wall time.
    ///
    /// Caller must hold [`Self::scheduler`] so remote wake flush and local
    /// timer accounting cannot race on this field.
    fn advance_account_epoch(last_accounted_ns: &mut u64, now_ns: u64) -> u64 {
        let elapsed = if *last_accounted_ns == 0 {
            0
        } else {
            now_ns.saturating_sub(*last_accounted_ns)
        };
        *last_accounted_ns = now_ns;
        elapsed
    }

    /// Charges pending wall time to the RQ's running entity before placement.
    ///
    /// NOHZ may leave a lone task without timer IRQs for a long time. Wakes and
    /// new publishes must flush that elapsed time into EEVDF `curr` before
    /// computing system V / PLACE_LAG.
    fn flush_running_runtime(&mut self, now_ns: u64) {
        let mut sched = self.scheduler.lock();
        let elapsed = Self::advance_account_epoch(&mut self.last_accounted_ns, now_ns);
        if elapsed == 0 {
            return;
        }
        #[cfg(feature = "sched_eevdf")]
        sched.account_curr_elapsed(elapsed);
        #[cfg(not(feature = "sched_eevdf"))]
        {
            let _ = elapsed;
        }
    }

    /// Sole writer of [`TaskInner::cpu_id`] for run-queue ownership.
    ///
    /// Semantics of the ownership field:
    /// - runnable / running: CPU whose run queue currently owns the task
    /// - blocked: last owner CPU, used as wake-affinity preference by
    ///   [`select_wake_run_queue`]
    ///
    /// Call sites outside `publish_task` / `enqueue_task` / `switch_to_local`
    /// are limited to boot bring-up before a run queue exists.
    #[cfg(feature = "smp")]
    #[inline]
    fn set_owner_cpu(task: &TaskInner, cpu_id: LogicalCpuId) {
        task.set_cpu_id(cpu_id);
    }

    /// Attach this run queue as the task's owner before publish / enqueue / local switch.
    ///
    /// If the task already counted toward another RQ's [`Self::nr_home`], that
    /// occupancy moves here (wake-fallback / migrate-in / idle-pull). Block does not call
    /// this, so a sleeping worker keeps its home.
    #[cfg(feature = "smp")]
    #[inline]
    fn attach_owner(&self, task: &KtaskRef) {
        let old = task.cpu_id();
        if old != self.cpu_id
            && task.clear_home_charged()
            && let Some(old_rq) = RUN_QUEUES.try_get(old.as_usize())
        {
            old_rq.nr_home.fetch_sub(1, Ordering::Relaxed);
        }
        Self::set_owner_cpu(task, self.cpu_id);
    }

    /// Includes `task` in this RQ's `nr_running` snapshot if not already charged.
    #[cfg(feature = "smp")]
    fn charge_rq_load(&self, task: &KtaskRef) {
        if task.mark_rq_load_charged() {
            self.nr_running.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Drops `task` from this RQ's `nr_running` snapshot if it was charged.
    ///
    /// No-op for `switch_to_local` helpers that were never published.
    #[cfg(feature = "smp")]
    fn discharge_rq_load(&self, task: &KtaskRef) {
        if task.clear_rq_load_charged() {
            self.nr_running.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// Includes `task` in this RQ's sticky-home occupancy if not already charged.
    #[cfg(feature = "smp")]
    fn charge_home(&self, task: &KtaskRef) {
        if task.mark_home_charged() {
            self.nr_home.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Drops `task` from this RQ's sticky-home occupancy if it was charged.
    #[cfg(feature = "smp")]
    fn discharge_home(&self, task: &KtaskRef) {
        if task.clear_home_charged() {
            self.nr_home.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// Publish a newly runnable task onto this run queue's scheduler.
    ///
    /// This is the only path that should call `Scheduler::add_task` from ktask.
    fn publish_task(&mut self, task: KtaskRef) {
        #[cfg(feature = "smp")]
        self.attach_owner(&task);
        #[cfg(feature = "smp")]
        self.charge_rq_load(&task);
        #[cfg(feature = "smp")]
        self.charge_home(&task);
        self.scheduler.lock().add_task(task);
    }

    /// Enqueue a non-current ready task onto this run queue (unblock / migrate-in).
    ///
    /// This is the only path that should call `Scheduler::enqueue_task` from ktask.
    /// Running tasks leave through `leave_current` on the yield/preempt/block/migrate/exit paths.
    #[cfg(feature = "smp")]
    fn enqueue_task(&mut self, task: KtaskRef) {
        self.enqueue_ready_task(task, false);
    }

    /// Enqueue a ready task, optionally arming WF_SYNC in the same lock as
    /// NEXT_BUDDY nomination (Linux `ttwu` / `check_preempt_wakeup`).
    fn enqueue_ready_task(&mut self, task: KtaskRef, is_wake_sync: bool) {
        #[cfg(feature = "smp")]
        self.attach_owner(&task);
        #[cfg(feature = "smp")]
        self.charge_rq_load(&task);
        #[cfg(feature = "smp")]
        self.charge_home(&task);
        let now_ns = khal::time::monotonic_time_nanos();
        let mut sched = self.scheduler.lock();
        let elapsed = Self::advance_account_epoch(&mut self.last_accounted_ns, now_ns);
        #[cfg(feature = "sched_eevdf")]
        sched.account_curr_elapsed(elapsed);
        #[cfg(not(feature = "sched_eevdf"))]
        {
            let _ = elapsed;
        }
        sched.enqueue_task(task);
        #[cfg(feature = "sched_eevdf")]
        if is_wake_sync {
            sched.mark_sync_wake_preempt();
        }
        #[cfg(not(feature = "sched_eevdf"))]
        {
            let _ = is_wake_sync;
        }
    }

    /// Attach a local helper task and switch to it without enqueueing.
    ///
    /// Used for per-CPU helpers such as the affinity migration task that are
    /// entered via `switch_to` rather than the ready queue. Under EEVDF, install
    /// the helper into `curr` here so later peer-preempt probes see it; idle
    /// never takes this path.
    #[cfg(feature = "smp")]
    fn switch_to_local(&mut self, prev_task: CurrentTask, next_task: KtaskRef) {
        #[cfg(feature = "smp")]
        self.attach_owner(&next_task);
        #[cfg(feature = "sched_eevdf")]
        self.scheduler.lock().sync_running_curr(&next_task);
        self.switch_to(prev_task, next_task);
    }

    /// Puts target task into current run queue with `Ready` state
    /// if its state matches `current_state` (except idle task).
    ///
    /// Used for non-current tasks (unblock). Running tasks use `leave_current`.
    /// `is_wake_sync` arms EEVDF WF_SYNC in the same scheduler lock as enqueue.
    ///
    /// Returns whether the transition was rejected, enqueued immediately, or
    /// handed to the CPU completing the task's switch-out.
    fn put_task_with_state(
        &mut self,
        task: KtaskRef,
        current_state: TaskState,
        is_wake_sync: bool,
        resched: bool,
    ) -> WakeEnqueue {
        #[cfg(not(feature = "smp"))]
        let _ = resched;
        // If the task's state matches `current_state`, set its state to `Ready` and
        // put it back to the run queue (except idle task).
        if task.transition_state(current_state, TaskState::Ready) && !task.is_idle() {
            #[cfg(feature = "smp")]
            {
                // Dekker handoff with `clear_prev_task_on_cpu`: store pending
                // flags, then load `on_cpu`. The matching side stores
                // `on_cpu=false` then swaps the flags. SeqCst atomics plus the
                // store→load fence in `arm_wake_enqueue` / below; SeqCst
                // store then SeqCst load of another location is not enough.
                self.attach_owner(&task);
                task.arm_wake_enqueue(is_wake_sync, resched);
                if task.on_cpu() {
                    return WakeEnqueue::Deferred;
                }
                if task.take_wake_enqueue().is_none() {
                    return WakeEnqueue::Deferred;
                }
            }
            // TODO: priority
            self.enqueue_ready_task(task, is_wake_sync);
            WakeEnqueue::Enqueued
        } else {
            WakeEnqueue::Rejected
        }
    }

    /// Core reschedule subroutine.
    /// Pick the next task to run and switch to it.
    fn resched(&mut self) {
        #[cfg(feature = "sched_stat")]
        sched_stat_inc(&sched_stat_cpu(self.cpu_id).resched);
        let next = self
            .scheduler
            .lock()
            .pick_next_task()
            .unwrap_or_else(|| current_idle_task().clone());
        #[cfg(feature = "smp")]
        let next = if next.is_idle() && self.idle_pull() {
            self.scheduler.lock().pick_next_task().unwrap_or(next)
        } else {
            next
        };
        #[cfg(feature = "sched_stat")]
        if next.is_idle() {
            sched_stat_inc(&sched_stat_cpu(self.cpu_id).pick_idle);
        }
        assert!(
            next.is_ready(),
            "next {} is not ready: {:?}",
            next.id_name(),
            next.state()
        );
        self.switch_to(current(), next);
    }

    /// Steal one Ready `!on_cpu` waiter from a busier CPU onto this idle RQ.
    ///
    /// Locks only the source scheduler (then this RQ's enqueue lock). Never
    /// dest-then-src: that deadlocks against remote wake. Never steals
    /// `on_cpu` tasks (yield/preempt are already in the ready tree). After
    /// enqueue, drop the waiter if its cpumask no longer contains this CPU.
    #[cfg(feature = "smp")]
    fn idle_pull(&mut self) -> bool {
        let dest = self.cpu_id.as_usize();
        let nr_cpus = kcpu_id_map::nr_cpus();
        let mut tried = KCpuMask::new();
        loop {
            let Some(src_idx) = find_idle_pull_src(dest, &tried, nr_cpus, |cpu| {
                RUN_QUEUES
                    .try_get(cpu)
                    .map(|rq| rq.nr_running.load(Ordering::Relaxed))
                    .unwrap_or(0)
            }) else {
                return false;
            };
            tried.set(src_idx, true);
            debug_assert_ne!(src_idx, dest);
            let Some(stolen) = get_run_queue(src_idx).steal_ready_for_idle_pull(self.cpu_id) else {
                continue;
            };
            if self.commit_idle_pull(stolen) {
                #[cfg(feature = "sched_stat")]
                sched_stat_inc(&sched_stat_cpu(self.cpu_id).idle_pull);
                return true;
            }
        }
    }

    /// Enqueue a stolen waiter, then drop it if affinity no longer allows dest.
    ///
    /// Steal to enqueue is not atomic: the task is Ready, on no RQ, with
    /// `cpu_id` still at src. `set_task_affinity` can exclude dest in that
    /// window. Recheck after `enqueue_task` so this CPU does not pick a task
    /// the new mask forbids.
    #[cfg(feature = "smp")]
    fn commit_idle_pull(&mut self, stolen: KtaskRef) -> bool {
        let dest = self.cpu_id.as_usize();
        self.enqueue_task(stolen.clone());
        if stolen.cpumask().get(dest) {
            return true;
        }
        let Some(removed) = self.scheduler.lock().remove_task(&stolen) else {
            return false;
        };
        self.discharge_rq_load(&removed);
        migrate_entry(removed);
        false
    }

    /// Dequeue a stealable waiter from this (source) RQ. Caller must not hold
    /// the destination scheduler lock.
    #[cfg(feature = "smp")]
    fn steal_ready_for_idle_pull(&mut self, dest_cpu: LogicalCpuId) -> Option<KtaskRef> {
        let dest_idx = dest_cpu.as_usize();
        let now_ns = khal::time::monotonic_time_nanos();
        let stolen = {
            let mut sched = self.scheduler.lock();
            let elapsed = Self::advance_account_epoch(&mut self.last_accounted_ns, now_ns);
            #[cfg(feature = "sched_eevdf")]
            sched.account_curr_elapsed(elapsed);
            #[cfg(not(feature = "sched_eevdf"))]
            {
                let _ = elapsed;
            }
            sched.steal_ready_task(|task| can_idle_pull_task(task, dest_idx))?
        };
        self.discharge_rq_load(&stolen);
        Some(stolen)
    }

    fn switch_to(&mut self, prev_task: CurrentTask, next_task: KtaskRef) {
        // Make sure that IRQs are disabled by kernel guard or other means.
        assert!(
            !karch::local_irq_enabled(),
            "IRQs must be disabled during scheduling"
        );
        #[cfg(feature = "smp")]
        debug_assert_eq!(
            next_task.cpu_id(),
            self.cpu_id,
            "next task {} owner cpu {} != run queue {}",
            next_task.id_name(),
            next_task.cpu_id().as_usize(),
            self.cpu_id.as_usize()
        );
        trace!(
            "context switch: {} -> {}",
            prev_task.id_name(),
            next_task.id_name()
        );

        let now_ns = khal::time::monotonic_time_nanos();

        #[cfg(feature = "preempt")]
        next_task.set_preempt_pending(false);
        next_task.set_state(TaskState::Running);
        self.account_next_worker_running(&next_task);

        let program_next_deadline = |rq: &mut Self, next: &KtaskRef, now: u64| {
            let next_rel = {
                let sched = rq.scheduler.lock();
                rq.last_accounted_ns = now;
                if next.is_idle() {
                    None
                } else {
                    sched.next_preemption_ns(next)
                }
            };
            // Immediate requests must arm pending on the *incoming* task: at
            // this point `current()` is still the outgoing task.
            program_sched_deadline_for(Some(next), now, next_rel);
            rearm_local_timer(earliest_deadline(rq.cpu_id), None);
        };

        if prev_task.ptr_eq(&next_task) {
            #[cfg(feature = "sched_stat")]
            sched_stat_inc(&sched_stat_cpu(this_cpu_id()).switch_same);
            program_next_deadline(self, &next_task, now_ns);
            return;
        }

        let cpu_id = this_cpu_id();

        // Fire the context switch tracepoint.
        #[cfg(feature = "sched_stat")]
        sched_stat_inc(&sched_stat_cpu(cpu_id).switch);
        fire_context_switch(prev_task.trace_id(), next_task.trace_id());

        // Claim the task as running, we do this before switching to it
        // such that any running task will have this set.
        #[cfg(feature = "smp")]
        {
            next_task.set_on_cpu(true);
            next_task.set_on_cpu_mask_bit(this_cpu_id());
        }

        if let Some(runtime) = next_task.user_runtime() {
            // Publish the CPU in the next mm before switching hardware state
            // so concurrent flushers can over-target but never miss this CPU.
            // We intentionally do not clear the previous mm here: code after
            // `switch_to()` runs only when the old task is scheduled again, so
            // eager clear-after-switch is not a sound timing point.
            runtime.set_user_mm_resident_cpu(cpu_id);
        }

        {
            if let Some(runtime) = prev_task.user_runtime() {
                runtime.on_leave()
            }
            if let Some(runtime) = next_task.user_runtime() {
                runtime.on_enter()
            }
        }

        // Program the schedule timer for the incoming task before switching.
        program_next_deadline(self, &next_task, now_ns);

        // SAFETY: scheduling owns both task contexts here, IRQs are disabled,
        // and the percpu scheduler/task pointers being updated are local to
        // the current CPU.
        unsafe {
            let prev_ctx_ptr = prev_task.ctx_mut_ptr();
            let next_ctx_ptr = next_task.ctx_mut_ptr();

            #[cfg(target_arch = "aarch64")]
            if let Some(root) = next_task
                .user_runtime()
                .and_then(|runtime| runtime.switch_page_table_root())
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

            let suspended_exception = khal::context::suspend_active_exception_context();
            CurrentTask::set_current(prev_task, next_task);

            (*prev_ctx_ptr).switch_to(&*next_ctx_ptr);
            khal::context::resume_active_exception_context(suspended_exception);

            // Current it's **next_task** running on this CPU, clear the `prev_task`'s `on_cpu` field
            // to indicate that it has finished its scheduling process and no longer running on this CPU.
            #[cfg(feature = "smp")]
            clear_prev_task_on_cpu(self);
        }
    }

    fn account_current_worker_sleeping(&mut self, curr: &CurrentTask) {
        let Some(context) = kwork::raw::WorkqueueTaskContextIf::current_work_context() else {
            return;
        };
        let (did_sleep, wake_plan) = context.worker_will_block_in_scheduler();
        if !did_sleep {
            return;
        }
        curr.mark_workerqueue_sleep_accounted();
        self.execute_system_worker_wake_plan(context, wake_plan);
    }

    fn account_next_worker_running(&mut self, next_task: &KtaskRef) {
        if !next_task.take_workerqueue_sleep_accounted() {
            return;
        }
        let Some((work_key, queue_key, pool_key, worker_id, worker_token)) =
            next_task.workerqueue_current_work_context()
        else {
            return;
        };
        kwork::raw::WorkqueueTaskContext::new(
            work_key,
            queue_key,
            pool_key,
            kwork::raw::WorkerId::new(worker_id),
            kwork::raw::WorkerExecutionToken::from_usize(worker_token),
        )
        .worker_did_resume();
    }

    fn execute_system_worker_wake_plan(
        &mut self,
        context: kwork::raw::WorkqueueTaskContext,
        plan: kwork::raw::WorkerWakePlan,
    ) {
        let Some(binding) = context.system_pool_binding() else {
            return;
        };
        debug_assert_eq!(
            binding.cpu_id(),
            self.cpu_id,
            "workerqueue scheduler wake plan should target the current run queue"
        );
        if let Some(worker_id) = plan.worker_to_wake
            && let Some(task) = crate::workqueue::system_worker_task_for_wake(
                binding.kind(),
                binding.cpu_id(),
                worker_id,
            )
        {
            self.unblock_workerqueue_task_from_scheduler(task);
        }
        if plan.should_wake_manager
            && let Some(task) =
                crate::workqueue::system_manager_task_for_wake(binding.kind(), binding.cpu_id())
        {
            self.unblock_workerqueue_task_from_scheduler(task);
        }
    }

    fn unblock_workerqueue_task_from_scheduler(&mut self, task: KtaskRef) {
        let task_id = task.trace_id();
        let is_wake_sync = current_may_uninit().is_some_and(|current| current.is_wake_sync());
        let wake_enqueue = self.put_task_with_state(task, TaskState::Blocked, is_wake_sync, true);
        if wake_enqueue == WakeEnqueue::Rejected {
            return;
        }
        #[cfg(feature = "sched_stat")]
        sched_stat_inc(&sched_stat_cpu(self.cpu_id).unblock_task);
        fire_task_wakeup(task_id);
        #[cfg(feature = "preempt")]
        if wake_enqueue == WakeEnqueue::Enqueued {
            request_resched_on(self);
        }
    }
}

fn poll_gc(cx: &mut Context<'_>, registrations: &mut PollRegistrations) -> Poll<()> {
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
        let mut context = registrations.context(cx);
        if context.register(current_wait_for_exit()).is_err() {
            drop(context);
            // Retry after yielding rather than sleeping without a registration
            // or busy-spinning on `wake_by_ref`.
            crate::yield_now();
            continue;
        }
        drop(context);

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
/// then enqueues the task through the ownership-aware entry.
#[cfg(feature = "smp")]
pub(crate) fn migrate_entry(migrated_task: KtaskRef) {
    let rq = select_run_queue::<kspin::NoPreemptIrqSave>(&migrated_task);
    let now_ns = khal::time::monotonic_time_nanos();
    rq.inner.flush_running_runtime(now_ns);
    rq.inner.enqueue_task(migrated_task);
    // Same kick as `add_task`: a NOHZ idle or lone-task destination may have
    // its schedule timer disarmed, so the migrated peer needs a resched probe
    // (local deadline refresh or remote IPI) to avoid indefinite starvation.
    #[cfg(feature = "preempt")]
    request_resched_on(rq.inner);
}

#[cfg(feature = "smp")]
fn spawn_affinity_migration_task(migrated: KtaskRef) -> KtaskRef {
    const MIGRATION_TASK_STACK_SIZE: usize = 4096;
    TaskInner::new_internal(
        move || migrate_entry(migrated),
        "migration-task".into(),
        MIGRATION_TASK_STACK_SIZE,
    )
    .into_arc()
}

#[cfg(feature = "smp")]
fn try_remove_ready_task(task: &KtaskRef) -> Option<KtaskRef> {
    let rq = task_run_queue::<kspin::NoPreemptIrqSave>(task);
    if !task.is_ready() {
        return None;
    }
    let removed = rq.inner.scheduler.lock().remove_task(task)?;
    rq.inner.discharge_rq_load(&removed);
    Some(removed)
}

/// After [`KtaskRef::set_cpumask`], move the task off any CPU the new mask
/// forbids.
///
/// - current: migrate immediately (same as historical `set_current_affinity`);
/// - ready on a forbidden RQ: dequeue and `migrate_entry`;
/// - ready but not queued (idle-pull in flight): wait until dest enqueue or
///   the task starts running, then migrate as above;
/// - running remotely: request resched and wait for the affinity migrate in
///   [`CurrentRunQueueRef::preempt_resched`];
/// - blocked/exited: mask-only (wake / teardown pick a legal CPU later).
///
/// Returns `false` when a remote running task cannot be migrated synchronously
/// (no preempt/IPI path, or still on a forbidden CPU after the wait).
#[cfg(feature = "smp")]
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

    // idle-pull leaves a Ready task off every RQ until dest `enqueue_task`.
    // `try_remove_ready_task` misses that window and must not fall through to
    // `false` (the documented failure is only a remote *running* migrate).
    while task.is_ready() && !cpumask.get(task.cpu_id().as_usize()) {
        if let Some(removed) = try_remove_ready_task(task) {
            migrate_entry(removed);
            break;
        }
        core::hint::spin_loop();
    }

    if task.is_running() && !cpumask.get(task.cpu_id().as_usize()) {
        #[cfg(all(feature = "preempt", feature = "ipi"))]
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
        #[cfg(not(all(feature = "preempt", feature = "ipi")))]
        {
            return false;
        }
    }

    // A Running→Ready race may leave the task queued on the old RQ; requeue.
    if !task.is_running()
        && !matches!(task.state(), TaskState::Blocked | TaskState::Exited)
        && !cpumask.get(task.cpu_id().as_usize())
        && let Some(removed) = try_remove_ready_task(task)
    {
        migrate_entry(removed);
    }

    cpumask.get(task.cpu_id().as_usize())
        || matches!(task.state(), TaskState::Blocked | TaskState::Exited)
}

/// Clear the `on_cpu` field of previous task running on this CPU.
#[cfg(feature = "smp")]
pub(crate) unsafe fn clear_prev_task_on_cpu(current_rq: &mut RunQueue) {
    // SAFETY: this is called on the CPU that owns `PREV_TASK`, after the
    // context switch completed and before another previous-task record is
    // installed.
    let prev_task = unsafe {
        PREV_TASK
            .current_ref_raw()
            .upgrade()
            .expect("Invalid prev_task pointer or prev_task has been dropped")
    };
    prev_task.set_on_cpu(false);
    // Matching Dekker store-load fence; see `TaskInner::arm_wake_enqueue`.
    core::sync::atomic::fence(Ordering::SeqCst);

    // Matching Dekker load: SeqCst swap after the SeqCst `on_cpu=false`
    // store. If the waker deferred because it still saw `on_cpu`, this swap
    // observes the pending flags and finishes the enqueue. If this swap sees
    // 0, the waker already claimed the flags (or never armed) and will enqueue.
    let Some((is_wake_sync, resched)) = prev_task.take_wake_enqueue() else {
        return;
    };
    debug_assert!(
        prev_task.is_ready(),
        "pending wake task {} is not ready: {:?}",
        prev_task.id_name(),
        prev_task.state()
    );

    let target_cpu = prev_task.cpu_id();
    if target_cpu == current_rq.cpu_id {
        current_rq.enqueue_ready_task(prev_task, is_wake_sync);
        #[cfg(feature = "preempt")]
        if resched {
            request_resched_on(current_rq);
        }
    } else {
        let target_rq = get_run_queue(target_cpu.as_usize());
        target_rq.enqueue_ready_task(prev_task, is_wake_sync);
        #[cfg(feature = "preempt")]
        if resched {
            request_resched_on(target_rq);
        }
    }
    #[cfg(not(feature = "preempt"))]
    let _ = resched;
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

#[cfg(unittest)]
mod tests_blocked_resched {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicUsize, Ordering};

    use kspin::{NoPreemptIrqSave, SpinNoIrq};
    use unittest::{assert_eq, def_test};

    use super::{current_run_queue, select_wake_run_queue};
    use crate::{current, spawn, yield_now};

    /// Direct `blocked_resched` path (not via `block_on`) with an explicit
    /// caller-owned strong ref — the contract any second caller must follow.
    #[def_test(serial)]
    fn blocked_resched_survives_with_caller_owned_ref() {
        static STARTED: AtomicUsize = AtomicUsize::new(0);
        static RESUMED: AtomicUsize = AtomicUsize::new(0);
        // `unittest::assert!` returns `TestResult`; cannot use it inside spawn.
        static STRONG_OK: AtomicUsize = AtomicUsize::new(0);

        STARTED.store(0, Ordering::Release);
        RESUMED.store(0, Ordering::Release);
        STRONG_OK.store(0, Ordering::Release);

        let waiter = spawn(move || {
            // into_raw current slot = 1; this clone is the required second.
            let keep_alive = current().clone();
            if Arc::strong_count(&keep_alive) > 1 {
                STRONG_OK.store(1, Ordering::Release);
            }

            let woke = SpinNoIrq::new(false);
            {
                let mut rq = current_run_queue::<NoPreemptIrqSave>();
                STARTED.store(1, Ordering::Release);
                rq.blocked_resched(woke.lock());
            }
            drop(keep_alive);
            RESUMED.store(1, Ordering::Release);
        });

        while STARTED.load(Ordering::Acquire) == 0 {
            yield_now();
        }
        // Wake may race the Running→Blocked transition; retry until resume.
        while RESUMED.load(Ordering::Acquire) == 0 {
            select_wake_run_queue::<NoPreemptIrqSave>(&waiter).unblock_task(waiter.clone(), true);
            yield_now();
        }
        assert_eq!(STRONG_OK.load(Ordering::Acquire), 1);
        assert_eq!(RESUMED.load(Ordering::Acquire), 1);
    }
}

#[cfg(all(feature = "smp", unittest))]
mod tests_wake_affinity {
    #[cfg(feature = "sched_stat")]
    use core::sync::atomic::Ordering;

    use kcpu_id_map::LogicalCpuId;
    use kspin::NoPreemptIrqSave;
    use unittest::{assert, assert_eq, def_test};

    #[cfg(feature = "sched_stat")]
    use super::sched_stat_cpu;
    use super::{
        WakeEnqueue, WakePlacement, can_idle_pull_task, find_idle_pull_src, find_idlest_cpu,
        select_idle_cpu, select_wake_run_queue, wake_affinity_select,
    };
    use crate::{KCpuMask, KtaskRef, TaskInner, TaskState};

    fn test_task_on(home: usize) -> KtaskRef {
        let task = TaskInner::new_internal(|| {}, "wake-aff".into(), 4096).into_arc();
        task.set_cpu_id(LogicalCpuId::new(home));
        task
    }

    #[def_test]
    fn select_idle_cpu_keeps_prev_when_idle() {
        if kcpu_id_map::nr_cpus() >= 2 {
            let mut mask = KCpuMask::new();
            mask.set(0, true);
            mask.set(1, true);
            let idx = select_idle_cpu(&mask, Some(1), |cpu| cpu == 1);
            assert_eq!(idx, Some(1));
        }
    }

    #[def_test]
    fn select_idle_cpu_seeks_sibling_when_prev_busy() {
        if kcpu_id_map::nr_cpus() >= 2 {
            let mut mask = KCpuMask::new();
            mask.set(0, true);
            mask.set(1, true);
            let idx = select_idle_cpu(&mask, Some(1), |cpu| cpu == 0);
            assert_eq!(idx, Some(0));
        }
    }

    #[def_test]
    fn select_idle_cpu_none_when_all_busy() {
        if kcpu_id_map::nr_cpus() >= 2 {
            let mut mask = KCpuMask::new();
            mask.set(0, true);
            mask.set(1, true);
            assert_eq!(select_idle_cpu(&mask, Some(1), |_| false), None);
        }
    }

    #[def_test]
    fn wake_affinity_stays_home_when_prev_idle() {
        if kcpu_id_map::nr_cpus() >= 2 {
            let task = test_task_on(1);
            let mut mask = KCpuMask::new();
            mask.set(0, true);
            mask.set(1, true);
            task.set_cpumask(mask);

            // CPU1 is typically idle in this unittest; if the test itself runs
            // there, SIS may pick another idle CPU — still a legal wake.
            let (idx, place) = wake_affinity_select(&task);
            match place {
                WakePlacement::PrevCpu => assert_eq!(idx, 1),
                WakePlacement::IdleSibling => assert!(idx != 1 && task.cpumask().get(idx)),
                WakePlacement::Fallback => panic!("home is still in cpumask"),
            }
        }
    }

    #[def_test]
    fn wake_affinity_falls_back_when_home_forbidden() {
        if kcpu_id_map::nr_cpus() >= 2 {
            let task = test_task_on(1);
            let mut mask = KCpuMask::new();
            mask.set(0, true);
            task.set_cpumask(mask);

            let (idx, place) = wake_affinity_select(&task);
            assert_eq!(idx, 0);
            assert!(matches!(
                place,
                WakePlacement::IdleSibling | WakePlacement::Fallback
            ));
        }
    }

    #[def_test]
    fn wake_of_switching_task_defers_enqueue_without_spinning() {
        let task = test_task_on(0);
        task.set_state(TaskState::Blocked);
        task.set_on_cpu(true);

        let result = select_wake_run_queue::<NoPreemptIrqSave>(&task)
            .inner
            .put_task_with_state(task.clone(), TaskState::Blocked, false, true);

        assert!(result == WakeEnqueue::Deferred);
        assert_eq!(task.take_wake_enqueue(), Some((false, true)));
        task.set_on_cpu(false);
    }

    #[cfg(feature = "sched_stat")]
    fn placement_counter(cpu: usize, place: WakePlacement) -> u64 {
        let stats = sched_stat_cpu(LogicalCpuId::new(cpu));
        match place {
            WakePlacement::PrevCpu => stats.wakeup_last_cpu.load(Ordering::Relaxed),
            WakePlacement::IdleSibling => stats.wakeup_idle_sibling.load(Ordering::Relaxed),
            WakePlacement::Fallback => stats.wakeup_fallback.load(Ordering::Relaxed),
        }
    }

    #[cfg(feature = "sched_stat")]
    #[def_test]
    fn select_wake_run_queue_counts_placement() {
        if kcpu_id_map::nr_cpus() >= 2 {
            let task = test_task_on(1);
            let mut all = KCpuMask::new();
            all.set(0, true);
            all.set(1, true);
            task.set_cpumask(all);

            let (idx, place) = wake_affinity_select(&task);
            let before = placement_counter(idx, place);
            {
                let _rq = select_wake_run_queue::<NoPreemptIrqSave>(&task);
            }
            assert_eq!(placement_counter(idx, place), before + 1);

            let mut only0 = KCpuMask::new();
            only0.set(0, true);
            task.set_cpumask(only0);
            let (idx0, place0) = wake_affinity_select(&task);
            assert_eq!(idx0, 0);
            let before0 = placement_counter(idx0, place0);
            {
                let _rq = select_wake_run_queue::<NoPreemptIrqSave>(&task);
            }
            assert_eq!(placement_counter(idx0, place0), before0 + 1);
        }
    }

    #[def_test]
    fn find_idlest_prefers_lighter_cpu() {
        if kcpu_id_map::nr_cpus() >= 2 {
            let mut mask = KCpuMask::new();
            mask.set(0, true);
            mask.set(1, true);
            let idx = find_idlest_cpu(&mask, 0, None, |cpu| match cpu {
                0 => Some(3),
                1 => Some(1),
                _ => None,
            });
            assert_eq!(idx, 1);
        }
    }

    #[def_test]
    fn find_idlest_round_robins_ties() {
        if kcpu_id_map::nr_cpus() >= 2 {
            let mut mask = KCpuMask::new();
            mask.set(0, true);
            mask.set(1, true);
            let first = find_idlest_cpu(&mask, 0, None, |_| Some(0));
            let second = find_idlest_cpu(&mask, 1, None, |_| Some(0));
            assert_eq!(first, 0);
            assert_eq!(second, 1);
        }
    }

    #[def_test]
    fn find_idlest_prefers_local_on_tie() {
        if kcpu_id_map::nr_cpus() >= 2 {
            let mut mask = KCpuMask::new();
            mask.set(0, true);
            mask.set(1, true);
            let idx = find_idlest_cpu(&mask, 0, Some(1), |_| Some(1));
            assert_eq!(idx, 1);
        }
    }

    #[def_test]
    fn find_idlest_ignores_heavier_prefer() {
        if kcpu_id_map::nr_cpus() >= 2 {
            let mut mask = KCpuMask::new();
            mask.set(0, true);
            mask.set(1, true);
            let idx = find_idlest_cpu(&mask, 0, Some(0), |cpu| match cpu {
                0 => Some(2),
                1 => Some(1),
                _ => None,
            });
            assert_eq!(idx, 1);
        }
    }

    /// Idle-first: a sleeper's CPU (`is_busy == false`) ranks ahead of a
    /// running CPU. Key is `(is_busy, nr_home)` as in [`super::spawn_idlest_key`].
    #[def_test]
    fn find_idlest_idle_beats_busy() {
        if kcpu_id_map::nr_cpus() >= 2 {
            let mut mask = KCpuMask::new();
            mask.set(0, true);
            mask.set(1, true);
            let idx = find_idlest_cpu(&mask, 0, Some(0), |cpu| match cpu {
                0 => Some((true, 1usize)),
                1 => Some((false, 1usize)),
                _ => None,
            });
            assert_eq!(idx, 1);
        }
    }

    /// Among idle CPUs, a vacant CPU ranks ahead of a sleeper's CPU.
    #[def_test]
    fn find_idlest_vacant_idle_beats_sleeper() {
        if kcpu_id_map::nr_cpus() >= 2 {
            let mut mask = KCpuMask::new();
            mask.set(0, true);
            mask.set(1, true);
            let idx = find_idlest_cpu(&mask, 0, Some(0), |cpu| match cpu {
                0 => Some((false, 1usize)),
                1 => Some((false, 0usize)),
                _ => None,
            });
            assert_eq!(idx, 1);
        }
    }

    #[def_test]
    fn find_idle_pull_src_picks_busiest() {
        if kcpu_id_map::nr_cpus() >= 2 {
            let tried = KCpuMask::new();
            let src = find_idle_pull_src(0, &tried, 2, |cpu| match cpu {
                0 => 4,
                1 => 3,
                _ => 0,
            });
            assert_eq!(src, Some(1));
        }
    }

    #[def_test]
    fn find_idle_pull_src_ignores_lone_and_tried() {
        if kcpu_id_map::nr_cpus() >= 2 {
            let tried = KCpuMask::new();
            assert_eq!(
                find_idle_pull_src(0, &tried, 2, |cpu| match cpu {
                    1 => 1,
                    _ => 0,
                }),
                None
            );

            let mut tried = KCpuMask::new();
            tried.set(1, true);
            assert_eq!(
                find_idle_pull_src(0, &tried, 2, |cpu| match cpu {
                    1 => 5,
                    _ => 0,
                }),
                None
            );
        }
    }

    #[def_test]
    fn can_idle_pull_task_rejects_on_cpu() {
        let task = test_task_on(1);
        let mut mask = KCpuMask::new();
        mask.set(0, true);
        mask.set(1, true);
        task.set_cpumask(mask);
        assert!(can_idle_pull_task(&task, 0));
        task.set_on_cpu(true);
        assert!(!can_idle_pull_task(&task, 0));
        task.set_on_cpu(false);
        let mut only_home = KCpuMask::new();
        only_home.set(1, true);
        task.set_cpumask(only_home);
        assert!(!can_idle_pull_task(&task, 0));
    }
}

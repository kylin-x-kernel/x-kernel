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
use core::sync::atomic::{AtomicU64, Ordering};
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
             wakeup_fallback={} add_task={} unblock={} local_resched={} remote_resched={} \
             remote_resched_fail={} tick_preempt={} timer_irq_sched={} timer_irq_soft={} \
             timer_irq_periodic={} timer_irq_stale={} preempt_check={} preempt_need={} \
             preempt_skip_disabled={} preempt_skip_exception={} preempt_resched={} \
             preempt_denied={} resched={} pick_idle={} switch={} switch_same={}\n",
            cpu,
            stats.select_task.load(Ordering::Relaxed),
            stats.select_wakeup.load(Ordering::Relaxed),
            stats.wakeup_last_cpu.load(Ordering::Relaxed),
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
             wakeup_fallback={} add_task={} unblock={} local_resched={} remote_resched={} \
             remote_resched_fail={} tick_preempt={} timer_irq_sched={} timer_irq_soft={} \
             timer_irq_periodic={} timer_irq_stale={} preempt_check={} preempt_need={} \
             preempt_skip_disabled={} preempt_skip_exception={} preempt_resched={} \
             preempt_denied={} resched={} pick_idle={} switch={} switch_same={}",
            cpu,
            stats.select_task.load(Ordering::Relaxed),
            stats.select_wakeup.load(Ordering::Relaxed),
            stats.wakeup_last_cpu.load(Ordering::Relaxed),
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
    #[cfg(feature = "sched_stat")]
    sched_stat_inc(&sched_stat_cpu(LogicalCpuId::new(eligible[nth])).select_task);
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

/// Sticky-home wake placement: `(run_queue_index, used_home)`.
///
/// Keeps the task's owner CPU when still allowed by `cpumask`; otherwise falls
/// back to ordinary affinity RR. Exposed to unittest so the branch is covered
/// without depending on `/proc/sched_stat` counter side effects alone.
#[cfg(feature = "smp")]
#[inline]
pub(crate) fn wake_affinity_select(task: &KtaskRef) -> (usize, bool) {
    let cpumask = task.cpumask();
    let home = task.cpu_id();
    if cpumask.get(home.as_usize()) {
        (home.as_usize(), true)
    } else {
        (select_run_queue_index(&cpumask), false)
    }
}

/// Selects a run queue for a task that is becoming runnable from a wakeup.
///
/// Sticky home wake affinity (schbench ping-pong). Any "home busy → idle"
/// overflow has repeatedly collapsed RPS (~450 → ~325); keep home unless the
/// task's cpumask no longer contains it.
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
        let (index, sticky_home) = wake_affinity_select(task);
        #[cfg(feature = "sched_stat")]
        {
            let stats = sched_stat_cpu(LogicalCpuId::new(index));
            sched_stat_inc(&stats.select_wakeup);
            if sticky_home {
                sched_stat_inc(&stats.wakeup_last_cpu);
            } else {
                sched_stat_inc(&stats.wakeup_fallback);
            }
        }
        let _ = sticky_home;
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
        &self,
        elapsed: u64,
        is_idle: bool,
        should_preempt: bool,
    ) {
        if is_idle {
            return;
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

            // Exited list owns the task; `leave_current(Exit)` clears scheduler
            // current accounting (e.g. EEVDF `curr`) and must not arm PLACE_LAG.
            self.inner
                .scheduler
                .lock()
                .leave_current(curr.clone(), CurrentDisposition::Exit);

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

        // Deactivate while still Running / current, before any concurrent wake
        // can requeue us. Fair schedulers snapshot lag for later enqueue PLACE_LAG.
        self.inner
            .scheduler
            .lock()
            .leave_current(curr.clone(), CurrentDisposition::Block);

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
    #[cfg(feature = "smp")]
    #[inline]
    fn attach_owner(&self, task: &KtaskRef) {
        Self::set_owner_cpu(task, self.cpu_id);
    }

    /// Publish a newly runnable task onto this run queue's scheduler.
    ///
    /// This is the only path that should call `Scheduler::add_task` from ktask.
    fn publish_task(&mut self, task: KtaskRef) {
        #[cfg(feature = "smp")]
        self.attach_owner(&task);
        self.scheduler.lock().add_task(task);
    }

    /// Enqueue a non-current ready task onto this run queue (unblock / migrate-in).
    ///
    /// This is the only path that should call `Scheduler::enqueue_task` from ktask.
    /// Running tasks leave through `leave_current` on the yield/preempt/block/migrate/exit paths.
    fn enqueue_task(&mut self, task: KtaskRef) {
        self.enqueue_ready_task(task, false);
    }

    /// Enqueue a ready task, optionally arming WF_SYNC in the same lock as
    /// NEXT_BUDDY nomination (Linux `ttwu` / `check_preempt_wakeup`).
    fn enqueue_ready_task(&mut self, task: KtaskRef, is_wake_sync: bool) {
        #[cfg(feature = "smp")]
        self.attach_owner(&task);
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
                // `on_cpu=false` then swaps the flags. SeqCst on both pairs
                // guarantees at least one side sees the other's store; RA
                // would allow both to miss (task Ready, on no queue).
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
    rq.inner.scheduler.lock().remove_task(task)
}

/// After [`KtaskRef::set_cpumask`], move the task off any CPU the new mask
/// forbids.
///
/// - current: migrate immediately (same as historical `set_current_affinity`);
/// - ready on a forbidden RQ: dequeue and `migrate_entry`;
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

    if let Some(removed) = try_remove_ready_task(task) {
        migrate_entry(removed);
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
    use super::{WakeEnqueue, select_wake_run_queue, wake_affinity_select};
    use crate::{KCpuMask, KtaskRef, TaskInner, TaskState};

    fn test_task_on(home: usize) -> KtaskRef {
        let task = TaskInner::new_internal(|| {}, "wake-aff".into(), 4096).into_arc();
        task.set_cpu_id(LogicalCpuId::new(home));
        task
    }

    #[def_test]
    fn wake_affinity_sticks_to_home_when_allowed() {
        if kcpu_id_map::nr_cpus() >= 2 {
            let task = test_task_on(1);
            let mut mask = KCpuMask::new();
            mask.set(0, true);
            mask.set(1, true);
            task.set_cpumask(mask);

            let (idx, sticky) = wake_affinity_select(&task);
            assert!(sticky);
            assert_eq!(idx, 1);
        }
    }

    #[def_test]
    fn wake_affinity_falls_back_when_home_forbidden() {
        if kcpu_id_map::nr_cpus() >= 2 {
            let task = test_task_on(1);
            let mut mask = KCpuMask::new();
            mask.set(0, true);
            task.set_cpumask(mask);

            let (idx, sticky) = wake_affinity_select(&task);
            assert!(!sticky);
            assert_eq!(idx, 0);
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
    #[def_test]
    fn select_wake_run_queue_counts_sticky_and_fallback() {
        if kcpu_id_map::nr_cpus() >= 2 {
            let task = test_task_on(1);
            let mut all = KCpuMask::new();
            all.set(0, true);
            all.set(1, true);
            task.set_cpumask(all);

            let before_last = sched_stat_cpu(LogicalCpuId::new(1))
                .wakeup_last_cpu
                .load(Ordering::Relaxed);
            {
                let _rq = select_wake_run_queue::<NoPreemptIrqSave>(&task);
            }
            let after_last = sched_stat_cpu(LogicalCpuId::new(1))
                .wakeup_last_cpu
                .load(Ordering::Relaxed);
            assert_eq!(after_last, before_last + 1);

            let mut only0 = KCpuMask::new();
            only0.set(0, true);
            task.set_cpumask(only0);
            let before_fb = sched_stat_cpu(LogicalCpuId::new(0))
                .wakeup_fallback
                .load(Ordering::Relaxed);
            {
                let _rq = select_wake_run_queue::<NoPreemptIrqSave>(&task);
            }
            let after_fb = sched_stat_cpu(LogicalCpuId::new(0))
                .wakeup_fallback
                .load(Ordering::Relaxed);
            assert_eq!(after_fb, before_fb + 1);
        }
    }
}

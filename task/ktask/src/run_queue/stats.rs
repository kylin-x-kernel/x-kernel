// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-CPU scheduler counters and `/proc/sched_stat` text.

#[cfg(feature = "sched_stat")]
use alloc::string::String;
#[cfg(feature = "sched_stat")]
use core::fmt::Write;
#[cfg(feature = "sched_stat")]
use core::sync::atomic::AtomicU64;
#[cfg(feature = "sched_stat")]
use core::sync::atomic::Ordering;

#[cfg(any(feature = "sched_stat", feature = "smp"))]
use kcpu_id_map::LogicalCpuId;
#[cfg(feature = "sched_stat")]
use khal::percpu::this_cpu_id;

#[cfg(feature = "sched_stat")]
pub(super) struct SchedCpuStats {
    pub(super) select_task: AtomicU64,
    pub(super) select_wakeup: AtomicU64,
    pub(super) wakeup_last_cpu: AtomicU64,
    pub(super) wakeup_idle_sibling: AtomicU64,
    pub(super) wakeup_fallback: AtomicU64,
    pub(super) add_task: AtomicU64,
    pub(super) unblock_task: AtomicU64,
    pub(super) local_resched: AtomicU64,
    pub(super) remote_resched: AtomicU64,
    pub(super) remote_resched_fail: AtomicU64,
    pub(super) tick_preempt: AtomicU64,
    /// Hardware timer IRQ classified as schedule-deadline due.
    pub(super) timer_irq_sched: AtomicU64,
    /// Hardware timer IRQ classified as soft-timer due.
    pub(super) timer_irq_soft: AtomicU64,
    /// Hardware timer IRQ classified as periodic-callback due.
    pub(super) timer_irq_periodic: AtomicU64,
    /// Hardware timer IRQ with no source due (clamp early / cancel race).
    pub(super) timer_irq_stale: AtomicU64,
    pub(super) preempt_check: AtomicU64,
    pub(super) preempt_need: AtomicU64,
    pub(super) preempt_skip_disabled: AtomicU64,
    pub(super) preempt_skip_exception: AtomicU64,
    pub(super) preempt_resched: AtomicU64,
    pub(super) preempt_denied: AtomicU64,
    pub(super) resched: AtomicU64,
    pub(super) pick_idle: AtomicU64,
    /// Successful idle-pull of a Ready `!on_cpu` waiter from a busier CPU.
    pub(super) idle_pull: AtomicU64,
    pub(super) switch: AtomicU64,
    pub(super) switch_same: AtomicU64,
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
pub(super) fn sched_stat_cpu(cpu_id: LogicalCpuId) -> &'static SchedCpuStats {
    &SCHED_CPU_STATS[cpu_id.as_usize()]
}

#[cfg(feature = "sched_stat")]
#[inline]
pub(super) fn sched_stat_inc(counter: &AtomicU64) {
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

#[cfg(all(feature = "smp", feature = "sched_stat"))]
#[inline]
pub(super) fn record_remote_resched(cpu_id: LogicalCpuId, failed: bool) {
    let stats = sched_stat_cpu(cpu_id);
    sched_stat_inc(&stats.remote_resched);
    if failed {
        sched_stat_inc(&stats.remote_resched_fail);
    }
}

#[cfg(all(feature = "smp", not(feature = "sched_stat")))]
#[inline]
pub(super) fn record_remote_resched(_cpu_id: LogicalCpuId, _failed: bool) {}

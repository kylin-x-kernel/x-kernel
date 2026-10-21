// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Spawn, wake, and idle-pull CPU placement.
//!
//! Chooses a destination CPU without taking a remote scheduler lock. `ksched`
//! only sees the already-chosen queue.

#[cfg(feature = "smp")]
use core::sync::atomic::{AtomicUsize, Ordering};

#[cfg(feature = "smp")]
use kcpu_id_map::KCpuMaskExt;
#[cfg(all(feature = "smp", feature = "sched_stat"))]
use kcpu_id_map::LogicalCpuId;
#[cfg(feature = "smp")]
use khal::percpu::this_cpu_id;
use kspin::BaseGuard;

#[cfg(not(feature = "smp"))]
use super::current_run_queue_ref;
#[cfg(feature = "smp")]
use super::get_run_queue;
#[cfg(all(feature = "smp", feature = "sched_stat"))]
use super::stats::{sched_stat_cpu, sched_stat_inc};
#[cfg(feature = "smp")]
use super::{KCpuMask, RUN_QUEUES, RunQueue};
use super::{KRunQueueRef, WakeRunQueueRef};
use crate::KtaskRef;

/// Selects the run queue for spawn / first publish / affinity migrate-in.
///
/// Uniprocessor: the only queue. SMP Fair: Linux `find_idlest_cpu`
/// (idle-first, then `nr_home`, then the creating CPU). SMP RT:
/// `find_lowest_rq` against `rt_preempt_rank`.
#[inline]
pub(crate) fn select_run_queue<G: BaseGuard>(task: &KtaskRef) -> KRunQueueRef<'static, G> {
    let irq_state = G::acquire();
    #[cfg(not(feature = "smp"))]
    {
        let _ = task;
        KRunQueueRef {
            inner: current_run_queue_ref(),
            state: irq_state,
            _phantom: core::marker::PhantomData,
        }
    }
    #[cfg(feature = "smp")]
    {
        // Fair stays on find_idlest. RT uses find_lowest_rq. Production tasks
        // stay Fair until sched_* calls Scheduler::set_attr.
        let index = if task.sched_class() == ksched::SchedClass::Rt {
            select_rt_cpu(task, this_cpu_id().as_usize(), &task.cpumask())
        } else {
            select_run_queue_index(&task.cpumask(), true)
        };
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
pub(super) enum WakePlacement {
    /// `prev_cpu` was idle, or no idle CPU existed so Linux keeps prev.
    /// RT also uses this when preempt-rank stays on home.
    PrevCpu,
    /// Home was busy; picked another `nr_running == 0` CPU.
    IdleSibling,
    /// Home not in `cpumask` and no idle CPU; spawn `find_idlest_cpu`.
    ///
    /// Fair only. An RT move off home is [`Self::RtRank`].
    Fallback,
    /// RT preempt-rank chose a CPU other than home.
    ///
    /// Home may still be allowed. This is the normal RT wake when home's
    /// runner is not preemptable, and also the pick when home is outside
    /// the mask. It is not the Fair `find_idlest_cpu` fallback.
    RtRank,
    /// `WF_SYNC` and this CPU's only runner is the waker. The wakee is placed
    /// on the waker CPU, which may differ from its previous `cpu_id`.
    SyncHandoff,
}

/// Whether this wake carries the `with_wake_sync` hint.
///
/// Hardirq and softirq leave the architecture current pointer on the
/// interrupted task, so that task's flag is not this wake. The context
/// snapshot runs only when the flag is set: one snapshot yields both
/// predicates, and ordinary wakes stay on the atomic load.
///
/// Known limit: the IRQ-exit tail drops the hardirq depth before deferred
/// work, and `wake_softirqd` runs after the softirq batch guard is gone. Both
/// predicates are then false while `current` is still the interrupted task, so
/// a task inside `with_wake_sync` leaks the hint onto the local ksoftirqd
/// wake. That daemon is pinned here, so placement does not move; the leak is
/// the EEVDF sync mark and `wakeup_sync_handoff`. Closing it needs a kirq
/// "left hardirq, not back in the interrupted task" predicate. Do not switch
/// this check to `is_in_interrupt_context`: that also drops a real hint from
/// a BH-disabled task, whose `current` is the waker.
#[inline]
pub(super) fn wake_carries_sync() -> bool {
    let Some(curr) = crate::current_may_uninit() else {
        return false;
    };
    if !curr.is_wake_sync() {
        return false;
    }
    let snapshot = kirq::context::irq_context_snapshot();
    !snapshot.is_in_hardirq() && !snapshot.is_serving_softirq()
}

/// Selects a run queue for a task becoming runnable from a wakeup.
///
/// Linux `select_idle_sibling`: prev if idle, else an idle CPU in `cpumask`.
/// The sync hint is read once here ([`wake_carries_sync`]) and travels with the
/// result. See [`wake_affinity_select`].
#[inline]
pub(crate) fn select_wake_run_queue<G: BaseGuard>(task: &KtaskRef) -> WakeRunQueueRef<G> {
    let irq_state = G::acquire();
    let is_wake_sync = wake_carries_sync();
    #[cfg(not(feature = "smp"))]
    {
        let _ = task;
        WakeRunQueueRef {
            rq: KRunQueueRef {
                inner: current_run_queue_ref(),
                state: irq_state,
                _phantom: core::marker::PhantomData,
            },
            is_wake_sync,
        }
    }
    #[cfg(feature = "smp")]
    {
        let (index, place) = wake_affinity_select(task, is_wake_sync);
        #[cfg(feature = "sched_stat")]
        {
            let stats = sched_stat_cpu(LogicalCpuId::new(index));
            sched_stat_inc(&stats.select_wakeup);
            match place {
                WakePlacement::PrevCpu => sched_stat_inc(&stats.wakeup_last_cpu),
                WakePlacement::IdleSibling => sched_stat_inc(&stats.wakeup_idle_sibling),
                WakePlacement::Fallback => sched_stat_inc(&stats.wakeup_fallback),
                WakePlacement::RtRank => sched_stat_inc(&stats.wakeup_rt_rank),
                WakePlacement::SyncHandoff => sched_stat_inc(&stats.wakeup_sync_handoff),
            }
        }
        #[cfg(not(feature = "sched_stat"))]
        let _ = place;
        WakeRunQueueRef {
            rq: KRunQueueRef {
                inner: get_run_queue(index),
                state: irq_state,
                _phantom: core::marker::PhantomData,
            },
            is_wake_sync,
        }
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
        RUN_QUEUES.try_get(idx).map(spawn_idlest_key)
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
pub(super) fn find_idlest_cpu<K: Copy + Ord>(
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
pub(super) fn find_idle_pull_src(
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
pub(super) fn can_idle_pull_task(task: &crate::KTask, dest_cpu: usize) -> bool {
    task.is_ready() && !task.on_cpu() && !task.is_idle() && task.cpumask().get(dest_cpu)
}

/// Linux `select_idle_sibling`: `prev` if idle, else the first idle CPU in `cpumask`.
///
/// `is_idle` is true when that CPU is schedulable and has `nr_running == 0`.
/// [`None`] means no idle CPU; the caller keeps `prev` when it is still allowed.
#[cfg(feature = "smp")]
pub(super) fn select_idle_cpu(
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

/// Waker CPU for a sync handoff, before idle search.
///
/// The hint only nominates a candidate (Linux `wake_affine_idle`:
/// `nr_running` includes the waker, so 1 means nobody else is queued). Any
/// placement rule may then veto it, and a vetoed wake falls through to the
/// ordinary idle search. Affinity is the only veto today. An idle prev must
/// not become one: that is the measured handoff (`lat_ctx` keeps the wakee
/// off an idle home). `wake_wide` and domain limits can still be vetoes here.
///
/// `is_wake_sync` is already [`wake_carries_sync`]: hardirq and softirq have
/// cleared the interrupted task's flag.
#[cfg(feature = "smp")]
pub(super) fn sync_handoff_target(
    is_wake_sync: bool,
    here_allowed: bool,
    nr_running: usize,
    here: usize,
) -> Option<usize> {
    let candidate = (is_wake_sync && nr_running == 1).then_some(here)?;
    here_allowed.then_some(candidate)
}

/// Sync handoff onto the waker CPU, ahead of idle search.
///
/// This is stronger than Linux: an idle prev does not override the candidate.
/// It suits peers that alternately block (`lat_ctx` 9.45 µs → 4.86 µs, because
/// the wakee no longer waits for an IPI to an idle home). A pipeline whose two
/// ends both compute can lose parallelism. That case is unmeasured and is an
/// accepted cost; do not veto it by testing whether prev is idle or local.
///
/// `is_wake_sync` is the single hint from [`wake_carries_sync`]. This function
/// does not read `current` or the IRQ context again.
#[cfg(feature = "smp")]
fn sync_handoff_cpu(cpumask: &KCpuMask, is_wake_sync: bool) -> Option<usize> {
    if !is_wake_sync {
        return None;
    }
    let here = this_cpu_id().as_usize();
    let nr_running = RUN_QUEUES.try_get(here)?.nr_running.load(Ordering::Relaxed);
    sync_handoff_target(is_wake_sync, cpumask.get(here), nr_running, here)
}

/// Wake placement analogue of Linux `select_idle_sibling`.
///
/// Sticky-home plus "home busy → idle" overflow collapsed schbench to ~3-way
/// parallelism (RPS ~450 → ~325) because the overflowed task never left.
/// Linux keeps prev when it is idle, otherwise seeks an idle CPU; the next
/// wake of the displaced worker can seek again (musical chairs, still ~4-way).
/// A `WF_SYNC` waker that is alone on this CPU is the exception: unless a
/// veto applies, the wakee takes this CPU ([`sync_handoff_cpu`]).
///
/// No idle CPU: stay on prev if allowed. Home forbidden: spawn `find_idlest`.
#[cfg(feature = "smp")]
#[inline]
pub(super) fn wake_affinity_select(task: &KtaskRef, is_wake_sync: bool) -> (usize, WakePlacement) {
    if task.sched_class() == ksched::SchedClass::Rt {
        return wake_rt(task);
    }
    let cpumask = task.cpumask();
    if let Some(cpu) = sync_handoff_cpu(&cpumask, is_wake_sync) {
        return (cpu, WakePlacement::SyncHandoff);
    }
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

/// Linux `select_task_rq_rt` / `find_lowest_rq`.
///
/// Stay on `hint` when this RT task can preempt the runner there. Otherwise
/// pick the allowed CPU with the weakest runner that is still preemptable.
/// If none, stay on `hint` when allowed so the waiter queues behind stronger RT.
#[cfg(feature = "smp")]
fn select_rt_cpu(task: &KtaskRef, hint: usize, cpumask: &KCpuMask) -> usize {
    let prio = task.rt_priority_snapshot();
    let index = find_lowest_rt_cpu(cpumask, hint, prio, |idx| {
        RUN_QUEUES
            .try_get(idx)
            .map(|rq| rq.rt_preempt_rank.load(Ordering::Relaxed))
    });
    #[cfg(feature = "sched_stat")]
    sched_stat_inc(&sched_stat_cpu(LogicalCpuId::new(index)).select_task);
    index
}

/// Stay on `hint` when `prio` can preempt it. Otherwise pick the weakest
/// still-preemptable runner. Equal ranks keep the first CPU in `cpumask`
/// order. If none can be preempted, stay on `hint` when it is schedulable.
///
/// `rank_of` is [`ksched::Scheduler::rt_preempt_rank`]: the runner, or 255
/// when that CPU's RT class is throttled. A higher RT waiter already queued
/// there does not raise the rank, so a mid-priority task can land behind it
/// when several CPUs share a rank. This pick does not track the highest
/// queued RT priority.
///
/// `rank_of` returns [`None`] for a CPU that is not yet schedulable.
///
/// # Panics
///
/// Panics when `cpumask` is empty.
#[cfg(feature = "smp")]
pub(super) fn find_lowest_rt_cpu(
    cpumask: &KCpuMask,
    hint: usize,
    prio: u8,
    rank_of: impl Fn(usize) -> Option<u8>,
) -> usize {
    assert!(!cpumask.is_empty(), "No available CPU for task execution");

    if cpumask.get(hint)
        && let Some(rank) = rank_of(hint)
        && prio > rank
    {
        return hint;
    }

    let mut best_cpu = None;
    let mut best_rank = u8::MAX;
    for cpu_id in cpumask.iter_logical() {
        let idx = cpu_id.as_usize();
        let Some(rank) = rank_of(idx) else {
            continue;
        };
        if prio <= rank {
            continue;
        }
        // `hint` is not in this scan: a preemptable hint already returned.
        // Equal ranks therefore follow `cpumask` iteration order.
        let is_better = best_cpu.is_none() || rank < best_rank;
        if is_better {
            best_rank = rank;
            best_cpu = Some(idx);
        }
    }
    if let Some(cpu) = best_cpu {
        return cpu;
    }
    if cpumask.get(hint) && rank_of(hint).is_some() {
        return hint;
    }
    cpumask
        .iter_logical()
        .map(|cpu| cpu.as_usize())
        .find(|idx| rank_of(*idx).is_some())
        .expect("No available CPU for task execution")
}

/// Wake CPU pick for RT. Hint is prev when allowed, otherwise the waker CPU.
#[cfg(feature = "smp")]
fn wake_rt(task: &KtaskRef) -> (usize, WakePlacement) {
    let cpumask = task.cpumask();
    let home = task.cpu_id().as_usize();
    let is_home_allowed = cpumask.get(home);
    let hint = if is_home_allowed {
        home
    } else {
        this_cpu_id().as_usize()
    };
    let cpu = select_rt_cpu(task, hint, &cpumask);
    let place = if is_home_allowed && cpu == home {
        WakePlacement::PrevCpu
    } else {
        WakePlacement::RtRank
    };
    (cpu, place)
}

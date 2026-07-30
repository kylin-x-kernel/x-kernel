// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Scheduler algorithm tests ported from StarryOS EEVDF scheduler.
//!
//! Covers FIFO, Round-Robin, CFS, and EEVDF schedulers plus the
//! per-CPU dispatcher and EEVDF fairness simulations.

#![cfg(unittest)]

extern crate alloc;

use alloc::{sync::Arc, vec::Vec};

use unittest::{assert, assert_eq, def_test};

use crate::*;

const SLICE: usize = 5;

// ============================================================================
// FIFO scheduler tests
// ============================================================================

#[def_test]
fn fifo_sched_order() {
    const NUM_TASKS: usize = 11;
    let mut scheduler = FifoScheduler::<usize>::new();
    for i in 0..NUM_TASKS {
        scheduler.add_task(Arc::new(FifoTask::new(i)));
    }
    for i in 0..NUM_TASKS * 10 - 1 {
        let next = scheduler.pick_next_task().unwrap();
        assert_eq!(*next.inner(), i % NUM_TASKS);
        scheduler.task_tick(&next);
        scheduler.put_prev_task(next, false);
    }
    let mut n = 0;
    while scheduler.pick_next_task().is_some() {
        n += 1;
    }
    assert_eq!(n, NUM_TASKS);
}

#[def_test]
fn fifo_remove() {
    const NUM_TASKS: usize = 100;
    let mut scheduler = FifoScheduler::<usize>::new();
    let mut tasks = Vec::new();
    for i in 0..NUM_TASKS {
        let t = Arc::new(FifoTask::new(i));
        tasks.push(t.clone());
        scheduler.add_task(t);
    }
    for i in (0..NUM_TASKS).rev() {
        let t = scheduler.remove_task(&tasks[i]).unwrap();
        assert_eq!(*t.inner(), i);
    }
}

// ============================================================================
// Round-Robin scheduler tests
// ============================================================================

#[def_test]
fn rr_sched_order() {
    const NUM_TASKS: usize = 11;
    let mut scheduler = RRScheduler::<usize, SLICE>::new();
    for i in 0..NUM_TASKS {
        scheduler.add_task(Arc::new(RRTask::new(i)));
    }
    for i in 0..NUM_TASKS * 10 - 1 {
        let next = scheduler.pick_next_task().unwrap();
        assert_eq!(*next.inner(), i % NUM_TASKS);
        scheduler.task_tick(&next);
        scheduler.put_prev_task(next, false);
    }
    let mut n = 0;
    while scheduler.pick_next_task().is_some() {
        n += 1;
    }
    assert_eq!(n, NUM_TASKS);
}

#[def_test]
fn rr_preempts_after_slice_expires() {
    let mut scheduler = RRScheduler::<usize, SLICE>::new();
    scheduler.add_task(Arc::new(RRTask::new(0)));
    let t = scheduler.pick_next_task().unwrap();
    for _ in 0..SLICE - 1 {
        assert!(!scheduler.task_tick(&t));
    }
    assert!(scheduler.task_tick(&t));
}

#[def_test]
fn rr_remove() {
    const NUM_TASKS: usize = 100;
    let mut scheduler = RRScheduler::<usize, SLICE>::new();
    let mut tasks = Vec::new();
    for i in 0..NUM_TASKS {
        let t = Arc::new(RRTask::new(i));
        tasks.push(t.clone());
        scheduler.add_task(t);
    }
    for i in (0..NUM_TASKS).rev() {
        let t = scheduler.remove_task(&tasks[i]).unwrap();
        assert_eq!(*t.inner(), i);
    }
}

// ============================================================================
// CFS scheduler tests
// ============================================================================

#[def_test]
fn cfs_sched_order() {
    const NUM_TASKS: usize = 11;
    let mut scheduler = CFScheduler::<usize>::new();
    for i in 0..NUM_TASKS {
        scheduler.add_task(Arc::new(CFSTask::new(i)));
    }
    for i in 0..NUM_TASKS * 10 - 1 {
        let next = scheduler.pick_next_task().unwrap();
        assert_eq!(*next.inner(), i % NUM_TASKS);
        scheduler.task_tick(&next);
        scheduler.put_prev_task(next, false);
    }
    let mut n = 0;
    while scheduler.pick_next_task().is_some() {
        n += 1;
    }
    assert_eq!(n, NUM_TASKS);
}

#[def_test]
fn cfs_remove() {
    const NUM_TASKS: usize = 100;
    let mut scheduler = CFScheduler::<usize>::new();
    let mut tasks = Vec::new();
    for i in 0..NUM_TASKS {
        let t = Arc::new(CFSTask::new(i));
        tasks.push(t.clone());
        scheduler.add_task(t);
    }
    for i in (0..NUM_TASKS).rev() {
        let t = scheduler.remove_task(&tasks[i]).unwrap();
        assert_eq!(*t.inner(), i);
    }
}

// ============================================================================
// EEVDF scheduler tests
// ============================================================================

#[def_test]
fn eevdf_sched_order() {
    const NUM_TASKS: usize = 11;
    let mut scheduler = EevdfScheduler::<usize, SLICE>::new();
    for i in 0..NUM_TASKS {
        scheduler.add_task(Arc::new(EevdfEntity::new(i)));
    }
    for i in 0..NUM_TASKS * 10 - 1 {
        let next = scheduler.pick_next_task().unwrap();
        assert_eq!(*next.inner(), i % NUM_TASKS);
        scheduler.task_tick(&next);
        scheduler.put_prev_task(next, false);
    }
    let mut n = 0;
    while scheduler.pick_next_task().is_some() {
        n += 1;
    }
    assert_eq!(n, NUM_TASKS);
}

#[def_test]
fn eevdf_remove() {
    const NUM_TASKS: usize = 100;
    let mut scheduler = EevdfScheduler::<usize, SLICE>::new();
    let mut tasks = Vec::new();
    for i in 0..NUM_TASKS {
        let t = Arc::new(EevdfEntity::new(i));
        tasks.push(t.clone());
        scheduler.add_task(t);
    }
    for i in (0..NUM_TASKS).rev() {
        let t = scheduler.remove_task(&tasks[i]).unwrap();
        assert_eq!(*t.inner(), i);
    }
}

#[def_test]
fn eevdf_high_weight_gets_earlier_deadline() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();
    let t_bg = Arc::new(EevdfEntity::new(1usize));
    let t_fg = Arc::new(EevdfEntity::new(2usize));

    sched.add_task(t_bg.clone());
    sched.add_task(t_fg.clone());
    sched.set_priority(&t_bg, 19);
    sched.set_priority(&t_fg, -20);

    let next = sched.pick_next_task().unwrap();
    assert_eq!(*next.inner(), 2);
}

#[def_test]
fn eevdf_eligible_preferred_over_earlier_deadline() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();
    let t1 = Arc::new(EevdfEntity::new(1usize));
    let t2 = Arc::new(EevdfEntity::new(2usize));

    sched.add_task(t1.clone());
    sched.add_task(t2.clone());

    let running = sched.pick_next_task().unwrap();
    assert_eq!(*running.inner(), 1);
    for _ in 0..4 {
        sched.task_tick(&running);
    }
    sched.put_prev_task(running, false);

    let next = sched.pick_next_task().unwrap();
    assert_eq!(*next.inner(), 2);
}

#[def_test]
fn eevdf_set_priority_rejects_out_of_range() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();
    let t = Arc::new(EevdfEntity::new(1usize));
    sched.add_task(t.clone());
    assert!(!sched.set_priority(&t, -21));
    assert!(!sched.set_priority(&t, 20));
}

#[def_test]
fn eevdf_set_priority_on_running_task() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();
    let t1 = Arc::new(EevdfEntity::new(1usize));
    let t2 = Arc::new(EevdfEntity::new(2usize));
    sched.add_task(t1.clone());
    sched.add_task(t2.clone());

    let running = sched.pick_next_task().unwrap();
    assert!(sched.set_priority(&running, 10));
    sched.put_prev_task(running, false);
    assert!(sched.pick_next_task().is_some());
}

#[def_test]
fn eevdf_preempted_keeps_deadline() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();
    let t = Arc::new(EevdfEntity::new(1usize));
    sched.add_task(t.clone());

    let running = sched.pick_next_task().unwrap();
    let dl_before = running.deadline();
    sched.task_tick(&running);
    sched.put_prev_task(running, true);

    let picked = sched.pick_next_task().unwrap();
    assert_eq!(picked.deadline(), dl_before);
}

#[def_test]
fn eevdf_preempted_expired_deadline_uses_remaining_slice() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();
    let t = Arc::new(EevdfEntity::new(1usize));
    sched.add_task(t.clone());

    let running = sched.pick_next_task().unwrap();
    sched.task_tick(&running);
    sched.task_tick(&running);
    assert_eq!(running.slice_for_test(), 3);

    let vr_now = running.vruntime_for_test();
    running.set_deadline_for_test(vr_now - 1);
    sched.put_prev_task(running, true);

    let picked = sched.pick_next_task().unwrap();
    let vr = picked.vruntime_for_test();
    let expected = vr + 3 * (1024isize * 1024 / 1024);
    assert_eq!(picked.deadline(), expected);
}

#[def_test]
fn eevdf_preempted_valid_deadline_not_overwritten() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();
    let t = Arc::new(EevdfEntity::new(1usize));
    sched.add_task(t.clone());

    let running = sched.pick_next_task().unwrap();
    let dl_before = running.deadline();
    sched.task_tick(&running);
    assert!(running.vruntime_for_test() < dl_before);
    sched.put_prev_task(running, true);

    let picked = sched.pick_next_task().unwrap();
    assert_eq!(picked.deadline(), dl_before);
}

#[def_test]
fn eevdf_deadline_preemption_triggers() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();
    let t1 = Arc::new(EevdfEntity::new(1usize));
    sched.add_task(t1.clone());
    let running = sched.pick_next_task().unwrap();
    assert!(!sched.task_tick(&running));

    let t2 = Arc::new(EevdfEntity::new(2usize));
    sched.add_task(t2.clone());
    sched.set_priority(&t2, -20);

    assert!(sched.task_tick(&running));
}

#[def_test]
fn eevdf_tick_preempts_eligible_non_head() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();
    let t_run = Arc::new(EevdfEntity::new(1usize));
    sched.add_task(t_run.clone());
    let running = sched.pick_next_task().unwrap();
    // Late deadline so an eligible ready task can beat us.
    running.set_deadline_for_test(1_000_000);

    // Global min-deadline task is ineligible (vruntime ≫ V).
    let t_head = Arc::new(EevdfEntity::new(2usize));
    t_head.set_vruntime_for_test(500_000);
    t_head.set_deadline_for_test(0);
    sched.inject_ready_for_test(t_head);

    // Eligible task with tighter deadline than current.
    let t_elig = Arc::new(EevdfEntity::new(3usize));
    t_elig.set_vruntime_for_test(0);
    t_elig.set_deadline_for_test(100);
    sched.inject_ready_for_test(t_elig);

    assert!(sched.task_tick(&running));
}

#[def_test]
fn eevdf_wake_places_positive_lag_task_first() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();
    let t1 = Arc::new(EevdfEntity::new(1usize));
    let t2 = Arc::new(EevdfEntity::new(2usize));
    sched.add_task(t1.clone());
    sched.add_task(t2.clone());

    // Run t1 for a full slice so it pulls ahead of t2.
    let a = sched.pick_next_task().unwrap();
    assert_eq!(*a.inner(), 1);
    for _ in 0..SLICE {
        let _ = sched.task_tick(&a);
    }
    sched.put_prev_task(a, false);

    // t2 is behind → positive lag on sleep.
    let b = sched.pick_next_task().unwrap();
    assert_eq!(*b.inner(), 2);
    sched.account_sleep(&b);
    assert!(b.needs_place_for_test());
    assert!(b.vlag_for_test() > 0);

    // Let t1 run further while t2 sleeps.
    let a2 = sched.pick_next_task().unwrap();
    assert_eq!(*a2.inner(), 1);
    for _ in 0..SLICE {
        let _ = sched.task_tick(&a2);
    }
    sched.put_prev_task(a2, false);

    // Wake t2 with PLACE_LAG; it should be selected next.
    sched.put_prev_task(b, false);
    assert!(!t2.needs_place_for_test());
    let next = sched.pick_next_task().unwrap();
    assert_eq!(*next.inner(), 2);
}

#[def_test]
fn eevdf_new_task_placement_must_include_current_in_virtual_time() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();

    // Running task becomes curr, then advances to 0x1400.
    let t_run = Arc::new(EevdfEntity::new(1usize));
    sched.add_task(t_run.clone());
    let curr = sched.pick_next_task().unwrap();
    assert!(Arc::ptr_eq(&curr, &t_run));
    curr.set_vruntime_for_test(0x1400);

    // Ready peer at vruntime 0 (equal weight).
    let t_ready = Arc::new(EevdfEntity::new(2usize));
    t_ready.set_vruntime_for_test(0);
    t_ready.set_deadline_for_test(1);
    sched.inject_ready_for_test(t_ready);

    // V = (0 + 0x1400) / 2 = 0xa00 when curr is included.
    let t_new = Arc::new(EevdfEntity::new(3usize));
    sched.add_task(t_new.clone());
    assert_eq!(t_new.vruntime_for_test(), 0xa00);
}

#[def_test]
fn eevdf_zero_lag_wake_placement_must_include_current() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();

    let t_run = Arc::new(EevdfEntity::new(1usize));
    sched.add_task(t_run.clone());
    let curr = sched.pick_next_task().unwrap();
    curr.set_vruntime_for_test(0x1400);

    let t_ready = Arc::new(EevdfEntity::new(2usize));
    t_ready.set_vruntime_for_test(0);
    t_ready.set_deadline_for_test(1);
    sched.inject_ready_for_test(t_ready);

    let t_wake = Arc::new(EevdfEntity::new(3usize));
    t_wake.set_vlag_for_test(0);
    t_wake.set_needs_place_for_test(true);
    sched.put_prev_task(t_wake.clone(), false);
    assert!(!t_wake.needs_place_for_test());
    assert_eq!(t_wake.vruntime_for_test(), 0xa00);
}

#[def_test]
fn eevdf_zero_lag_wake_with_only_curr_places_at_curr_vruntime() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();

    let t_run = Arc::new(EevdfEntity::new(1usize));
    sched.add_task(t_run.clone());
    let curr = sched.pick_next_task().unwrap();
    curr.set_vruntime_for_test(0x1400);

    let t_wake = Arc::new(EevdfEntity::new(2usize));
    t_wake.set_vlag_for_test(0);
    t_wake.set_needs_place_for_test(true);
    sched.put_prev_task(t_wake.clone(), false);
    assert_eq!(t_wake.vruntime_for_test(), 0x1400);
}

#[def_test]
fn eevdf_shorter_request_gets_earlier_deadline() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();
    let t_long = Arc::new(EevdfEntity::new(1usize));
    let t_short = Arc::new(EevdfEntity::new(2usize));
    assert!(t_short.set_request_ticks(1));
    sched.add_task(t_long.clone());
    sched.add_task(t_short.clone());

    let next = sched.pick_next_task().unwrap();
    assert_eq!(*next.inner(), 2);
    assert!(t_short.deadline() < t_long.deadline());
}

// ============================================================================
// EEVDF stats tests
// ============================================================================

#[def_test]
fn eevdf_stats_count_preemption_and_pick() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();
    sched.set_stats_enabled(true);

    let t1 = Arc::new(EevdfEntity::new(1usize));
    sched.add_task(t1.clone());
    let running = sched.pick_next_task().unwrap();
    assert_eq!(sched.stats().picks_total, 1);
    assert!(!sched.task_tick(&running));

    let t2 = Arc::new(EevdfEntity::new(2usize));
    sched.add_task(t2.clone());
    sched.set_priority(&t2, -20);
    assert!(sched.task_tick(&running));
    assert_eq!(sched.stats().preempt_by_deadline, 1);
}

#[def_test]
fn eevdf_stats_count_slice_expired() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();
    sched.set_stats_enabled(true);

    let t = Arc::new(EevdfEntity::new(1usize));
    sched.add_task(t);
    let running = sched.pick_next_task().unwrap();
    for _ in 0..SLICE - 1 {
        assert!(!sched.task_tick(&running));
    }
    assert!(sched.task_tick(&running));
    assert_eq!(sched.stats().slice_expired, 1);
}

#[def_test]
fn eevdf_stats_count_fallback_no_eligible() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();
    sched.set_stats_enabled(true);
    sched.set_debug_force_no_eligible(true);

    let t1 = Arc::new(EevdfEntity::new(1usize));
    let t2 = Arc::new(EevdfEntity::new(2usize));
    sched.add_task(t1);
    sched.add_task(t2);
    let _ = sched.pick_next_task().unwrap();

    let stats = sched.stats();
    assert_eq!(stats.picks_total, 1);
    assert_eq!(stats.fallback_no_eligible, 1);
}

#[def_test]
fn eevdf_stats_disabled_does_not_count() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();

    let t1 = Arc::new(EevdfEntity::new(1usize));
    let t2 = Arc::new(EevdfEntity::new(2usize));
    sched.add_task(t1.clone());
    sched.add_task(t2.clone());
    let running = sched.pick_next_task().unwrap();
    let _ = sched.task_tick(&running);
    sched.put_prev_task(running, false);
    let _ = sched.pick_next_task().unwrap();

    let stats = sched.stats();
    assert_eq!(stats.picks_total, 0);
    assert_eq!(stats.preempt_by_deadline, 0);
    assert_eq!(stats.slice_expired, 0);
    assert_eq!(stats.fallback_no_eligible, 0);
}

#[def_test]
fn eevdf_stats_reset_clears_counters() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();
    sched.set_stats_enabled(true);

    let t1 = Arc::new(EevdfEntity::new(1usize));
    sched.add_task(t1.clone());
    let running = sched.pick_next_task().unwrap();
    for _ in 0..SLICE {
        let _ = sched.task_tick(&running);
    }

    let before = sched.stats();
    assert!(before.picks_total > 0 || before.slice_expired > 0);
    sched.reset_stats();

    let after = sched.stats();
    assert_eq!(after.picks_total, 0);
    assert_eq!(after.preempt_by_deadline, 0);
    assert_eq!(after.slice_expired, 0);
    assert_eq!(after.fallback_no_eligible, 0);
}

// ============================================================================
// Per-CPU scheduler tests
// ============================================================================

struct Task {
    id: u64,
}

impl Task {
    fn new(id: u64) -> Arc<Self> {
        Arc::new(Self { id })
    }
}

impl crate::per_cpu::HasSchedulerId for Task {
    fn sched_id(&self) -> u64 {
        self.id
    }
}

#[def_test]
fn percpu_fifo_picks_in_insertion_order() {
    use crate::per_cpu::{PerCpuScheduler, SchedulerKind};

    let mut s = PerCpuScheduler::<Task>::new(SchedulerKind::Fifo);
    for id in 0..5u64 {
        s.add_task(Task::new(id));
    }
    for expected in 0..5u64 {
        let t = s.pick_next_task().unwrap();
        assert_eq!(t.id, expected);
        s.put_prev_task(t, false);
    }
}

#[def_test]
fn percpu_fifo_never_preempts() {
    use crate::per_cpu::{PerCpuScheduler, SchedulerKind};

    let mut s = PerCpuScheduler::<Task>::new(SchedulerKind::Fifo);
    s.add_task(Task::new(0));
    let t = s.pick_next_task().unwrap();
    for _ in 0..100 {
        assert!(!s.task_tick(&t));
    }
    s.put_prev_task(t, false);
}

#[def_test]
fn percpu_rr_preempts_after_slice() {
    use crate::per_cpu::{PerCpuScheduler, SchedulerKind};

    let mut s = PerCpuScheduler::<Task>::new(SchedulerKind::Rr);
    s.add_task(Task::new(0));
    let t = s.pick_next_task().unwrap();
    let mut preempted = false;
    for _ in 0..10 {
        if s.task_tick(&t) {
            preempted = true;
            break;
        }
    }
    assert!(preempted);
    s.put_prev_task(t, true);
}

#[def_test]
fn percpu_eevdf_fairness_equal_weight() {
    use crate::per_cpu::{PerCpuScheduler, SchedulerKind};

    let mut s = PerCpuScheduler::<Task>::new(SchedulerKind::Eevdf);
    let tasks: Vec<_> = (0..3u64).map(Task::new).collect();
    for t in &tasks {
        s.add_task(t.clone());
    }

    let mut counts = [0u64; 3];
    const TICKS: u64 = 6000;
    for _ in 0..TICKS {
        let t = s.pick_next_task().unwrap();
        let id = t.id as usize;
        let preempt = s.task_tick(&t);
        counts[id] += 1;
        s.put_prev_task(t, preempt);
    }

    let expected = TICKS / 3;
    for &got in counts.iter() {
        let err = (got as f64 - expected as f64).abs() / expected as f64;
        assert!(err < 0.05);
    }
}

#[def_test]
fn percpu_task_migrates_between_schedulers() {
    use crate::per_cpu::{PerCpuScheduler, SchedulerKind};

    let mut eevdf = PerCpuScheduler::<Task>::new(SchedulerKind::Eevdf);
    let mut fifo = PerCpuScheduler::<Task>::new(SchedulerKind::Fifo);

    let t0 = Task::new(0);
    let t1 = Task::new(1);
    eevdf.add_task(t0.clone());
    eevdf.add_task(t1.clone());
    for _ in 0..20 {
        let t = eevdf.pick_next_task().unwrap();
        let preempt = eevdf.task_tick(&t);
        eevdf.put_prev_task(t, preempt);
    }

    let migrated = eevdf.remove_task(&t0).unwrap();
    fifo.add_task(migrated);
    let picked = fifo.pick_next_task().unwrap();
    assert_eq!(picked.id, 0);
    fifo.put_prev_task(picked, false);

    let remaining = eevdf.pick_next_task().unwrap();
    assert_eq!(remaining.id, 1);
}

// ============================================================================
// EEVDF fairness simulation tests
// ============================================================================

fn eevdf_simulate(
    task_nice: &[(Arc<EevdfEntity<usize, SLICE>>, isize)],
    total_ticks: u64,
) -> Vec<u64> {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();
    for (task, nice) in task_nice {
        sched.add_task(task.clone());
        sched.set_priority(task, *nice);
    }

    let n = task_nice.len();
    let mut counts = alloc::vec![0u64; n];
    let mut current = sched.pick_next_task().unwrap();
    let mut elapsed = 0u64;

    loop {
        counts[*current.inner()] += 1;
        elapsed += 1;
        let should_switch = sched.task_tick(&current);
        if should_switch || elapsed >= total_ticks {
            let preempt = should_switch && current.slice_for_test() > 0;
            sched.put_prev_task(current, preempt);
            if elapsed >= total_ticks {
                break;
            }
            current = sched.pick_next_task().unwrap();
        }
    }
    counts
}

#[def_test]
fn eevdf_equal_weight_share_cpu_evenly() {
    const N: usize = 3;
    const TOTAL: u64 = 9_000;

    let tasks: Vec<_> = (0..N)
        .map(|i| (Arc::new(EevdfEntity::<usize, SLICE>::new(i)), 0isize))
        .collect();
    let counts = eevdf_simulate(&tasks, TOTAL);

    let expected = TOTAL / N as u64;
    for &got in counts.iter() {
        let err = (got as f64 - expected as f64).abs() / expected as f64;
        assert!(err < 0.05);
    }
}

#[def_test]
fn eevdf_weighted_tasks_get_proportional_cpu() {
    const TOTAL: u64 = 15_000;
    let nice_vals = [-5isize, 0, 5];
    let weights = [3121isize, 1024, 335];
    let total_weight: isize = weights.iter().sum();

    let tasks: Vec<_> = (0..3)
        .map(|i| (Arc::new(EevdfEntity::<usize, SLICE>::new(i)), nice_vals[i]))
        .collect();
    let counts = eevdf_simulate(&tasks, TOTAL);

    for (&got, &w) in counts.iter().zip(weights.iter()) {
        let expected = TOTAL as f64 * w as f64 / total_weight as f64;
        let err = (got as f64 - expected).abs() / expected;
        assert!(err < 0.10);
    }
}

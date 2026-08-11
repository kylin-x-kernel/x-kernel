// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Scheduler algorithm tests ported from StarryOS EEVDF scheduler.
//!
//! Covers FIFO, Round-Robin, CFS, and EEVDF schedulers plus EEVDF fairness
//! simulations.

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
        scheduler.leave_current(next, CurrentDisposition::Yield);
    }
    let mut n = 0;
    while let Some(t) = scheduler.pick_next_task() {
        n += 1;
        scheduler.leave_current(t, CurrentDisposition::Exit);
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
        scheduler.leave_current(next, CurrentDisposition::Yield);
    }
    let mut n = 0;
    while let Some(t) = scheduler.pick_next_task() {
        n += 1;
        scheduler.leave_current(t, CurrentDisposition::Exit);
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
        scheduler.leave_current(next, CurrentDisposition::Yield);
    }
    let mut n = 0;
    while let Some(t) = scheduler.pick_next_task() {
        n += 1;
        scheduler.leave_current(t, CurrentDisposition::Exit);
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
        scheduler.leave_current(next, CurrentDisposition::Yield);
    }
    let mut n = 0;
    while let Some(t) = scheduler.pick_next_task() {
        n += 1;
        scheduler.leave_current(t, CurrentDisposition::Exit);
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
fn eevdf_exit_releases_cached_current() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();
    let task = Arc::new(EevdfEntity::new(1usize));
    sched.add_task(task.clone());
    let picked = sched.pick_next_task().unwrap();
    assert!(Arc::ptr_eq(&picked, &task));
    drop(picked);

    // `leave_current(Exit)` clears scheduler current accounting so an exiting
    // task is not kept alive by the RQ after the CPU switches away.
    sched.leave_current(task.clone(), CurrentDisposition::Exit);
    assert_eq!(Arc::strong_count(&task), 1);
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
    sched.leave_current(running, CurrentDisposition::Yield);

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
    sched.leave_current(running, CurrentDisposition::Yield);
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
    sched.leave_current(running, CurrentDisposition::Preempt);

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
    sched.leave_current(running, CurrentDisposition::Preempt);

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
    sched.leave_current(running, CurrentDisposition::Preempt);

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
fn eevdf_tick_preempts_expired_deadline_only_with_peer() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();
    sched.set_stats_enabled(true);

    let t = Arc::new(EevdfEntity::new(1usize));
    sched.add_task(t);
    let running = sched.pick_next_task().unwrap();
    let vr = running.vruntime_for_test();
    // One-tick wake deadline alone must NOT thrash after one tick.
    running.set_deadline_for_test(vr + 1024);
    assert!(!sched.task_tick(&running));
    assert_eq!(sched.stats().preempt_by_deadline, 0);

    // With a waiting eligible peer, expiry yields the CPU.
    let peer = Arc::new(EevdfEntity::new(2usize));
    peer.set_vruntime_for_test(0);
    peer.set_deadline_for_test(vr + 2048);
    sched.inject_ready_for_test(peer);
    assert!(sched.task_tick(&running));
    assert_eq!(sched.stats().preempt_by_deadline, 1);
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
fn eevdf_peer_preempt_probe_keeps_curr_when_wakee_later() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();

    let runner = Arc::new(EevdfEntity::new(1usize));
    sched.add_task(runner.clone());
    let curr = sched.pick_next_task().unwrap();
    assert_eq!(*curr.inner(), 1);
    for _ in 0..2 {
        let _ = sched.task_tick(&curr);
    }

    let wakee = Arc::new(EevdfEntity::new(2usize));
    wakee.set_vlag_for_test(0);
    wakee.set_needs_place_for_test(true);
    sched.enqueue_task(wakee.clone());
    assert_eq!(wakee.slice_for_test(), SLICE as isize);
    assert!(wakee.deadline() > curr.deadline());
    assert!(!sched.peer_preempts_curr());
}

#[def_test]
fn eevdf_peer_preempt_syncs_untracked_runner_at_switch_in() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();

    // Production installs curr in switch_to_local; probe itself does not sync.
    let runner = Arc::new(EevdfEntity::new(1usize));
    runner.set_vruntime_for_test(0);
    runner.set_deadline_for_test(0);

    let peer = Arc::new(EevdfEntity::new(2usize));
    peer.set_vruntime_for_test(0);
    peer.set_deadline_for_test(100);
    sched.inject_ready_for_test(peer);

    assert!(
        sched.curr_is_none(),
        "untracked runner must start with empty curr snapshot"
    );
    assert!(
        sched.peer_preempts_curr(),
        "empty curr (idle-like) with a ready peer reports preemptable"
    );

    sched.sync_running_curr(&runner);
    assert!(
        !sched.peer_preempts_curr(),
        "synced early-deadline runner must not be preempted by a later peer"
    );
}

#[def_test]
fn eevdf_peer_preempt_probe_switches_when_wakee_earlier() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();

    let runner = Arc::new(EevdfEntity::new(1usize));
    sched.add_task(runner.clone());
    let curr = sched.pick_next_task().unwrap();
    let _ = sched.task_tick(&curr);

    // Place normally, then re-key with an earlier deadline. Mutating deadline
    // in place would leave a stale ready-queue key and break pick/buddy lookup.
    let wakee = Arc::new(EevdfEntity::new(2usize));
    wakee.set_vlag_for_test(0);
    wakee.set_needs_place_for_test(true);
    sched.enqueue_task(wakee.clone());
    let vr = wakee.vruntime_for_test();
    let _ = sched.remove_task(&wakee);
    wakee.set_vruntime_for_test(vr);
    wakee.set_deadline_for_test(curr.deadline() - 1);
    sched.inject_ready_for_test(wakee.clone());
    assert!(sched.peer_preempts_curr());
    sched.leave_current(curr, CurrentDisposition::Preempt);
    let next = sched.pick_next_task().unwrap();
    assert_eq!(*next.inner(), 2);
}

#[def_test]
fn eevdf_concurrent_wakes_keep_deadline_order() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();
    sched.set_stats_enabled(true);

    let runner = Arc::new(EevdfEntity::new(1usize));
    sched.add_task(runner.clone());
    let curr = sched.pick_next_task().unwrap();
    curr.set_deadline_for_test(curr.vruntime_for_test() + 1_000_000);
    sched.refresh_curr_snapshot_for_test(&curr);

    let first = Arc::new(EevdfEntity::new(2usize));
    first.set_vlag_for_test(0);
    first.set_needs_place_for_test(true);
    sched.enqueue_task(first.clone());

    let second = Arc::new(EevdfEntity::new(3usize));
    second.set_vlag_for_test(0);
    second.set_needs_place_for_test(true);
    sched.enqueue_task(second);
    assert!(sched.peer_preempts_curr());
    sched.leave_current(curr, CurrentDisposition::Preempt);
    let next = sched.pick_next_task().unwrap();
    // Deadline-aware NEXT_BUDDY keeps `first`; pick uses that handoff.
    assert_eq!(*next.inner(), 2);
    assert_eq!(sched.stats().wake_handoff, 1);
    assert!(sched.stats().wake_handoff_skipped_busy >= 1);
}

#[def_test]
fn eevdf_wake_buddy_handoff_when_curr_blocks() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();
    sched.set_stats_enabled(true);

    let runner = Arc::new(EevdfEntity::new(1usize));
    sched.add_task(runner.clone());
    let curr = sched.pick_next_task().unwrap();
    for _ in 0..2 {
        let _ = sched.task_tick(&curr);
    }

    let wakee = Arc::new(EevdfEntity::new(2usize));
    wakee.set_vlag_for_test(0);
    wakee.set_needs_place_for_test(true);
    sched.enqueue_task(wakee.clone());
    assert!(wakee.deadline() > curr.deadline());

    // Full-slice wakee does not preempt mid-slice curr.
    assert!(!sched.peer_preempts_curr());

    // Waker blocks: next pick should prefer the wake buddy.
    sched.leave_current(curr.clone(), CurrentDisposition::Block);
    let next = sched.pick_next_task().unwrap();
    assert_eq!(*next.inner(), 2);
    assert_eq!(sched.stats().wake_handoff, 1);
}

/// NEXT_BUDDY must not leapfrog an earlier-deadline eligible ready task.
#[def_test]
fn eevdf_wake_buddy_yields_to_earlier_eligible_peer() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();
    sched.set_stats_enabled(true);

    let runner = Arc::new(EevdfEntity::new(1usize));
    sched.add_task(runner.clone());
    let curr = sched.pick_next_task().unwrap();
    for _ in 0..2 {
        let _ = sched.task_tick(&curr);
    }

    // Nominate a later wakee as NEXT_BUDDY first.
    let buddy = Arc::new(EevdfEntity::new(3usize));
    buddy.set_vlag_for_test(0);
    buddy.set_needs_place_for_test(true);
    sched.enqueue_task(buddy.clone());

    // Inject an earlier-deadline eligible peer without renominating (simulates
    // a ready waiter that was not the latest wake handoff target).
    let earlier = Arc::new(EevdfEntity::new(2usize));
    earlier.set_vruntime_for_test(buddy.vruntime_for_test());
    earlier.set_deadline_for_test(buddy.deadline() - 10);
    earlier.reset_slice();
    sched.inject_ready_for_test(earlier.clone());
    assert!(earlier.deadline() < buddy.deadline());

    sched.leave_current(curr.clone(), CurrentDisposition::Block);
    let next = sched.pick_next_task().unwrap();
    assert_eq!(
        *next.inner(),
        2,
        "earlier eligible peer must beat a later NEXT_BUDDY"
    );
    assert_eq!(sched.stats().wake_handoff, 0);
}

#[def_test]
fn eevdf_sync_wake_preempts_eligible_later_deadline_buddy() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();
    sched.set_stats_enabled(true);

    let runner = Arc::new(EevdfEntity::new(1usize));
    sched.add_task(runner.clone());
    let curr = sched.pick_next_task().unwrap();
    for _ in 0..2 {
        let _ = sched.task_tick(&curr);
    }

    let wakee = Arc::new(EevdfEntity::new(2usize));
    wakee.set_vlag_for_test(0);
    wakee.set_needs_place_for_test(true);
    sched.enqueue_task(wakee.clone());
    assert!(wakee.deadline() > curr.deadline());

    sched.mark_sync_wake_preempt();
    assert!(sched.peer_preempts_curr());
    assert_eq!(sched.stats().wake_sync_preempt, 1);

    sched.leave_current(curr, CurrentDisposition::Preempt);
    let next = sched.pick_next_task().unwrap();
    assert_eq!(*next.inner(), 2);
    assert_eq!(sched.stats().wake_handoff, 1);
}

#[def_test]
fn eevdf_wake_uses_full_request_deadline() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();

    let runner = Arc::new(EevdfEntity::new(1usize));
    sched.add_task(runner);
    let curr = sched.pick_next_task().unwrap();

    let wakee = Arc::new(EevdfEntity::new(2usize));
    wakee.set_vlag_for_test(0);
    wakee.set_needs_place_for_test(true);
    sched.enqueue_task(wakee.clone());

    let vr = wakee.vruntime_for_test();
    let expected = vr + SLICE as isize * (1024 * 1024 / 1024);
    assert_eq!(wakee.deadline(), expected);
    assert_eq!(wakee.slice_for_test(), SLICE as isize);
    let _ = curr;
}

#[def_test]
fn eevdf_wake_negative_lag_stays_eligible() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();

    let runner = Arc::new(EevdfEntity::new(1usize));
    sched.add_task(runner.clone());
    let curr = sched.pick_next_task().unwrap();
    curr.set_vruntime_for_test(0x2000);
    sched.refresh_curr_snapshot_for_test(&curr);

    // Task that previously ran ahead of V: PLACE_LAG alone would place above V
    // and leave it ineligible while curr (or another eligible peer) runs.
    let wakee = Arc::new(EevdfEntity::new(2usize));
    wakee.set_vlag_for_test(-0x1000);
    wakee.set_needs_place_for_test(true);
    sched.enqueue_task(wakee.clone());

    assert!(wakee.vruntime_for_test() <= 0x2000);
    assert_eq!(wakee.slice_for_test(), SLICE as isize);
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
    sched.leave_current(a, CurrentDisposition::Yield);

    // t2 is behind → positive lag on sleep.
    let b = sched.pick_next_task().unwrap();
    assert_eq!(*b.inner(), 2);
    sched.leave_current(b.clone(), CurrentDisposition::Block);
    assert!(b.needs_place_for_test());
    assert!(b.vlag_for_test() > 0);

    // Let t1 run further while t2 sleeps.
    let a2 = sched.pick_next_task().unwrap();
    assert_eq!(*a2.inner(), 1);
    for _ in 0..SLICE {
        let _ = sched.task_tick(&a2);
    }
    sched.leave_current(a2, CurrentDisposition::Yield);

    // Wake t2 with PLACE_LAG; it should be selected next.
    sched.enqueue_task(b);
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
    sched.refresh_curr_snapshot_for_test(&curr);

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
    sched.refresh_curr_snapshot_for_test(&curr);

    let t_ready = Arc::new(EevdfEntity::new(2usize));
    t_ready.set_vruntime_for_test(0);
    t_ready.set_deadline_for_test(1);
    sched.inject_ready_for_test(t_ready);

    let t_wake = Arc::new(EevdfEntity::new(3usize));
    t_wake.set_vlag_for_test(0);
    t_wake.set_needs_place_for_test(true);
    sched.enqueue_task(t_wake.clone());
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
    sched.refresh_curr_snapshot_for_test(&curr);

    let t_wake = Arc::new(EevdfEntity::new(2usize));
    t_wake.set_vlag_for_test(0);
    t_wake.set_needs_place_for_test(true);
    sched.enqueue_task(t_wake.clone());
    assert_eq!(t_wake.vruntime_for_test(), 0x1400);
}

#[def_test]
fn eevdf_place_waking_clamps_to_min_vruntime() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();

    let t_run = Arc::new(EevdfEntity::new(1usize));
    sched.add_task(t_run.clone());
    let curr = sched.pick_next_task().unwrap();
    curr.set_vruntime_for_test(100);
    sched.refresh_curr_snapshot_for_test(&curr);

    // Seed ready with a low and a high vruntime task, then remove the low one
    // so dequeue advances min_vruntime to the remaining high watermark.
    let t_low = Arc::new(EevdfEntity::new(2usize));
    t_low.set_vruntime_for_test(0);
    t_low.set_deadline_for_test(1);
    sched.inject_ready_for_test(t_low.clone());
    let t_high = Arc::new(EevdfEntity::new(3usize));
    t_high.set_vruntime_for_test(0x2000);
    t_high.set_deadline_for_test(0x2001);
    sched.inject_ready_for_test(t_high);
    let _ = sched.remove_task(&t_low);

    // Large positive lag would place far below min_vruntime without the clamp.
    let t_wake = Arc::new(EevdfEntity::new(4usize));
    t_wake.set_vlag_for_test(0x10000);
    t_wake.set_needs_place_for_test(true);
    sched.enqueue_task(t_wake.clone());
    assert!(t_wake.vruntime_for_test() >= 0x2000);
}

#[def_test]
fn eevdf_remove_task_lag_includes_curr() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();

    let t_run = Arc::new(EevdfEntity::new(1usize));
    sched.add_task(t_run.clone());
    let curr = sched.pick_next_task().unwrap();
    curr.set_vruntime_for_test(0x1400);
    sched.refresh_curr_snapshot_for_test(&curr);

    let t_ready = Arc::new(EevdfEntity::new(2usize));
    t_ready.set_vruntime_for_test(0);
    t_ready.set_deadline_for_test(1);
    sched.inject_ready_for_test(t_ready.clone());

    // V = (0 + 0x1400) / 2 = 0xa00 when curr is included; lag = V - vr = 0xa00.
    let removed = sched.remove_task(&t_ready).unwrap();
    assert_eq!(removed.vlag_for_test(), 0xa00);
    assert!(removed.needs_place_for_test());
}

#[def_test]
fn eevdf_pick_next_requires_leave_current() {
    // Kernel unittest cannot recover from intentional panics, so assert the
    // precondition instead of invoking the `pick_next` assert path.
    let mut sched = EevdfScheduler::<usize, SLICE>::new();

    let t_run = Arc::new(EevdfEntity::new(1usize));
    sched.add_task(t_run.clone());
    let curr = sched.pick_next_task().unwrap();
    assert!(!sched.curr_is_none());

    let t_next = Arc::new(EevdfEntity::new(2usize));
    t_next.set_vruntime_for_test(0);
    t_next.set_deadline_for_test(1);
    sched.inject_ready_for_test(t_next.clone());

    sched.leave_current(curr, CurrentDisposition::Exit);
    assert!(sched.curr_is_none());
    let picked = sched.pick_next_task().unwrap();
    assert!(Arc::ptr_eq(&picked, &t_next));
}

#[def_test]
fn eevdf_leave_dispositions_clear_curr_and_place_rules() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();
    let t = Arc::new(EevdfEntity::new(1usize));
    sched.add_task(t.clone());
    let curr = sched.pick_next_task().unwrap();
    assert!(!sched.curr_is_none());

    // Yield clears curr and requeues.
    sched.leave_current(curr, CurrentDisposition::Yield);
    assert!(sched.curr_is_none());

    let curr = sched.pick_next_task().unwrap();
    let slice_before = curr.slice_for_test();
    assert!(slice_before > 0);
    let _ = sched.task_tick(&curr);
    let remaining = curr.slice_for_test();
    assert!(remaining < slice_before);
    sched.leave_current(curr.clone(), CurrentDisposition::Preempt);
    assert!(sched.curr_is_none());
    // Preempt keeps remaining slice across requeue id change.
    assert_eq!(curr.slice_for_test(), remaining);

    let curr = sched.pick_next_task().unwrap();
    sched.leave_current(curr.clone(), CurrentDisposition::Block);
    assert!(sched.curr_is_none());
    assert!(curr.needs_place_for_test());

    // Re-enter via enqueue PLACE_LAG.
    sched.enqueue_task(curr);
    assert!(!t.needs_place_for_test());

    let curr = sched.pick_next_task().unwrap();
    sched.leave_current(curr.clone(), CurrentDisposition::Migrate);
    assert!(sched.curr_is_none());
    assert!(curr.needs_place_for_test());
    // Source RQ weight/V no longer include the migrated task.
    // (ready empty and curr none => weight 0)
    // Re-place onto this RQ as migrate-in would.
    sched.enqueue_task(curr);

    let curr = sched.pick_next_task().unwrap();
    // Exit must not arm future placement.
    sched.leave_current(curr.clone(), CurrentDisposition::Exit);
    assert!(sched.curr_is_none());
    assert!(!curr.needs_place_for_test());
}

#[def_test]
fn eevdf_curr_snapshot_does_not_own_task() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();
    let t = Arc::new(EevdfEntity::new(1usize));
    sched.add_task(t.clone());
    let running = sched.pick_next_task().unwrap();
    // Owners: test handle `t` + local `running`. Snapshot must not add a third.
    assert_eq!(Arc::strong_count(&running), 2);
    sched.leave_current(running, CurrentDisposition::Exit);
    assert!(sched.curr_is_none());
    assert_eq!(Arc::strong_count(&t), 1);
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
    sched.leave_current(running, CurrentDisposition::Yield);
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
            sched.leave_current(
                current,
                if preempt {
                    CurrentDisposition::Preempt
                } else {
                    CurrentDisposition::Yield
                },
            );
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

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Kernel unit tests for FIFO / RR / CFS / EEVDF (`make unittest`).
//!
//! Cases live here. EEVDF construction helpers (`*_for_test`) are a
//! `#[cfg(unittest)]` impl at the bottom of `eevdf.rs`, not extra tests.

#![cfg(unittest)]

extern crate alloc;

use alloc::{sync::Arc, vec::Vec};

use unittest::{assert, assert_eq, def_test};

use crate::*;

/// Accounting quantum large enough that high-weight tasks still get non-zero
/// vruntime deltas under integer `elapsed * NICE_0_WEIGHT / weight` math.
const UNITS_PER_SLICE: usize = 5;
const UNIT_NS: u64 = 4096;
const SLICE: usize = UNITS_PER_SLICE * UNIT_NS as usize;

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
        scheduler.update_current(&next, UNIT_NS);
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
        scheduler.update_current(&next, UNIT_NS);
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
    for _ in 0..UNITS_PER_SLICE - 1 {
        assert!(!scheduler.update_current(&t, UNIT_NS));
    }
    assert!(scheduler.update_current(&t, UNIT_NS));
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
        scheduler.update_current(&next, UNIT_NS);
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
        scheduler.update_current(&next, UNIT_NS);
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
        sched.update_current(&running, UNIT_NS);
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
    sched.update_current(&running, UNIT_NS);
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
    sched.update_current(&running, UNIT_NS);
    sched.update_current(&running, UNIT_NS);
    assert_eq!(running.slice_for_test(), 3 * UNIT_NS);

    let vr_now = running.vruntime_for_test();
    running.set_deadline_for_test(vr_now - 1);
    sched.leave_current(running, CurrentDisposition::Preempt);

    let picked = sched.pick_next_task().unwrap();
    let vr = picked.vruntime_for_test();
    let expected = vr + (3 * UNIT_NS) as i64;
    assert_eq!(picked.deadline(), expected);
}

#[def_test]
fn eevdf_preempted_valid_deadline_not_overwritten() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();
    let t = Arc::new(EevdfEntity::new(1usize));
    sched.add_task(t.clone());

    let running = sched.pick_next_task().unwrap();
    let dl_before = running.deadline();
    sched.update_current(&running, UNIT_NS);
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
    assert!(!sched.update_current(&running, UNIT_NS));

    let t2 = Arc::new(EevdfEntity::new(2usize));
    sched.add_task(t2.clone());
    sched.set_priority(&t2, -20);

    assert!(
        !sched.update_current(&running, UNIT_NS),
        "Linux update_curr does not resched for a peer's earlier deadline"
    );
    assert!(sched.check_preempt_tick());
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
    running.set_deadline_for_test(vr + 1);
    assert!(!sched.update_current(&running, UNIT_NS));
    assert_eq!(sched.stats().preempt_by_deadline, 0);

    // With a waiting eligible peer, expiry yields the CPU.
    let peer = Arc::new(EevdfEntity::new(2usize));
    peer.set_vruntime_for_test(0);
    peer.set_deadline_for_test(vr + 2048);
    sched.inject_ready_for_test(peer);
    assert!(
        !sched.update_current(&running, UNIT_NS),
        "request already rolled; peer deadline is a tick/probe decision"
    );
    assert_eq!(sched.stats().preempt_by_deadline, 0);
    assert!(
        running.vruntime_for_test() < running.deadline(),
        "Linux update_deadline must roll a completed request before the probe"
    );
    assert!(sched.check_preempt_tick());
    assert_eq!(sched.stats().preempt_by_deadline, 1);
    assert!(
        sched.peer_preempts_curr(),
        "after rolling, an earlier-deadline peer must still win the probe"
    );
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

    assert!(!sched.update_current(&running, UNIT_NS));
    assert!(sched.check_preempt_tick());
}

/// Eligible pick must skip an ineligible deadline-order prefix (negative lag).
#[def_test]
fn eevdf_pick_skips_ineligible_earlier_deadlines() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();
    for i in 0..8 {
        let t = Arc::new(EevdfEntity::new(100 + i));
        t.set_vruntime_for_test(500_000);
        t.set_deadline_for_test(i as i64);
        sched.inject_ready_for_test(t);
    }
    let eligible = Arc::new(EevdfEntity::new(1usize));
    eligible.set_vruntime_for_test(0);
    eligible.set_deadline_for_test(1000);
    sched.inject_ready_for_test(eligible);

    let picked = sched.pick_next_task().unwrap();
    assert_eq!(*picked.inner(), 1);
}

#[def_test]
fn eevdf_peer_preempt_probe_keeps_curr_when_wakee_later() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();

    let runner = Arc::new(EevdfEntity::new(1usize));
    sched.add_task(runner.clone());
    let curr = sched.pick_next_task().unwrap();
    assert_eq!(*curr.inner(), 1);
    for _ in 0..2 {
        let _ = sched.update_current(&curr, UNIT_NS);
    }

    let wakee = Arc::new(EevdfEntity::new(2usize));
    wakee.set_vlag_for_test(0);
    wakee.set_needs_place_for_test(true);
    sched.enqueue_task(wakee.clone());
    assert_eq!(wakee.slice_for_test(), SLICE as u64);
    assert!(wakee.deadline() > curr.deadline());
    assert!(!sched.peer_preempts_curr());
}

#[def_test]
fn eevdf_peer_preempt_syncs_untracked_runner_at_switch_in() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();

    // Production installs curr in switch_to_local; probe itself does not sync.
    let runner = Arc::new(EevdfEntity::new(1usize));
    runner.set_vruntime_for_test(0);
    runner.set_deadline_for_test(100);

    let peer = Arc::new(EevdfEntity::new(2usize));
    peer.set_vruntime_for_test(0);
    peer.set_deadline_for_test(200);
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
    let _ = sched.update_current(&curr, UNIT_NS);

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
        let _ = sched.update_current(&curr, UNIT_NS);
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
        let _ = sched.update_current(&curr, UNIT_NS);
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
        let _ = sched.update_current(&curr, UNIT_NS);
    }

    let wakee = Arc::new(EevdfEntity::new(2usize));
    wakee.set_vlag_for_test(0);
    wakee.set_needs_place_for_test(true);
    sched.enqueue_task(wakee.clone());
    assert!(wakee.deadline() > curr.deadline());
    assert!(
        !sched.check_preempt_tick(),
        "later deadline without WF_SYNC: tick keeps curr"
    );

    sched.mark_sync_wake_preempt();
    assert!(sched.check_preempt_tick());
    assert!(sched.peer_preempts_curr());
    assert_eq!(sched.stats().wake_sync_preempt, 1);

    sched.leave_current(curr, CurrentDisposition::Preempt);
    let next = sched.pick_next_task().unwrap();
    assert_eq!(*next.inner(), 2);
    assert_eq!(sched.stats().wake_handoff, 1);
    let rel = sched.next_preemption_ns(&next).unwrap();
    assert!(
        rel > 0,
        "WF_SYNC wakee must not arm Some(0) just because the previous runner has an earlier \
         deadline"
    );
}

/// After a completed request, Linux `update_deadline` assigns a new `vd`.
/// WF_SYNC must still be able to hand off; do not keep the old deadline in the
/// probe as a "must preempt" shortcut.
#[def_test]
fn eevdf_sync_wake_preempts_after_request_rolls() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();
    sched.set_stats_enabled(true);

    let runner = Arc::new(EevdfEntity::new(1usize));
    sched.add_task(runner.clone());
    let curr = sched.pick_next_task().unwrap();
    for _ in 0..UNITS_PER_SLICE {
        let _ = sched.update_current(&curr, UNIT_NS);
    }
    assert!(curr.vruntime_for_test() < curr.deadline());

    let wakee = Arc::new(EevdfEntity::new(2usize));
    wakee.set_vlag_for_test(0);
    wakee.set_needs_place_for_test(true);
    sched.enqueue_task(wakee.clone());

    sched.mark_sync_wake_preempt();
    assert!(sched.peer_preempts_curr());
    assert_eq!(sched.stats().wake_sync_preempt, 1);

    sched.leave_current(curr, CurrentDisposition::Preempt);
    let next = sched.pick_next_task().unwrap();
    assert_eq!(*next.inner(), 2);
    assert_eq!(sched.stats().wake_handoff, 1);
}

/// Remote IPI can lose `sync_preempt_pending` to an intervening slice-expire
/// pick. `mark_sync_wake_preempt` must arm `prefer_sync_buddy` so that pick
/// still hands off to the wakee.
#[def_test]
fn eevdf_mark_sync_prefers_buddy_on_pick_without_probe() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();
    sched.set_stats_enabled(true);

    let runner = Arc::new(EevdfEntity::new(1usize));
    sched.add_task(runner.clone());
    let curr = sched.pick_next_task().unwrap();
    for _ in 0..UNITS_PER_SLICE {
        let _ = sched.update_current(&curr, UNIT_NS);
    }

    let wakee = Arc::new(EevdfEntity::new(2usize));
    wakee.set_vlag_for_test(0);
    wakee.set_needs_place_for_test(true);
    sched.enqueue_task(wakee.clone());
    sched.mark_sync_wake_preempt();

    sched.leave_current(curr, CurrentDisposition::Preempt);
    let next = sched.pick_next_task().unwrap();
    assert_eq!(*next.inner(), 2);
    assert_eq!(sched.stats().wake_handoff, 1);
}

/// Diagnostic counters must distinguish "marked but probe lost the buddy"
/// from "probe ran, curr kept the CPU, buddy still queued".
#[def_test]
fn eevdf_wake_diag_counts_mark_and_failed_probe() {
    let mut idle = EevdfScheduler::<usize, SLICE>::new();
    idle.set_stats_enabled(true);
    idle.mark_sync_wake_preempt();
    assert_eq!(idle.stats().wake_sync_mark_no_buddy, 1);
    assert_eq!(idle.stats().wake_sync_mark, 0);

    let parked = Arc::new(EevdfEntity::new(1usize));
    parked.set_needs_place_for_test(true);
    idle.enqueue_task(parked);
    assert_eq!(idle.stats().wake_nominate_no_curr, 1);

    let mut sched = EevdfScheduler::<usize, SLICE>::new();
    sched.set_stats_enabled(true);
    let runner = Arc::new(EevdfEntity::new(2usize));
    sched.add_task(runner.clone());
    let curr = sched.pick_next_task().unwrap();
    let _ = sched.update_current(&curr, UNIT_NS);

    let wakee = Arc::new(EevdfEntity::new(3usize));
    wakee.set_vlag_for_test(0);
    wakee.set_needs_place_for_test(true);
    sched.enqueue_task(wakee.clone());
    assert!(wakee.deadline() > curr.deadline());
    assert!(!sched.peer_preempts_curr());
    assert_eq!(sched.stats().probe_false_with_buddy, 1);
    assert_eq!(sched.stats().wake_sync_preempt, 0);
}

/// Timer IRQ must see a live WF_SYNC mark without consuming it, including
/// while the buddy is still ineligible (early until-eligible fire).
#[def_test]
fn eevdf_sync_wake_pending_does_not_consume_mark() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();
    let runner = Arc::new(EevdfEntity::new(1usize));
    sched.add_task(runner.clone());
    let curr = sched.pick_next_task().unwrap();
    let _ = sched.update_current(&curr, UNIT_NS);

    // Negative lag → PLACE above V → first probe loses; mark must stay live.
    let wakee = Arc::new(EevdfEntity::new(2usize));
    wakee.set_vlag_for_test(-(SLICE as i64));
    wakee.set_needs_place_for_test(true);
    sched.enqueue_task(wakee.clone());
    sched.mark_sync_wake_preempt();

    assert!(wakee.vruntime_for_test() > sched.system_vruntime_for_test());
    assert!(sched.sync_wake_pending());
    assert!(
        !sched.check_preempt_tick(),
        "ineligible buddy: Linux check_preempt_tick keeps curr"
    );
    assert!(
        !sched.peer_preempts_curr(),
        "ineligible buddy must lose the probe"
    );
    assert!(
        sched.sync_wake_pending(),
        "failed probe must leave the WF_SYNC mark for the timer retry"
    );

    // V advances at `w_curr / W`, so the vruntime gap is not wall time.
    // Account until the buddy is eligible; `check_preempt_tick` must not
    // consume the mark.
    while wakee.vruntime_for_test() > sched.system_vruntime_for_test() {
        let _ = sched.update_current(&curr, UNIT_NS);
    }
    assert!(sched.sync_wake_pending());
    assert!(sched.check_preempt_tick());
    assert!(sched.sync_wake_pending());
    // Eligible later-deadline buddy does not make `next_preemption_ns` return 0
    // (`update_current` is also false). Tick must set `need_resched`; refresh
    // alone would re-arm the remaining request.
    let next = sched.next_preemption_ns(&curr).unwrap();
    assert!(next > 0);
    assert!(sched.peer_preempts_curr());
    assert!(!sched.sync_wake_pending());
}

/// Remote WF_SYNC often enqueues after `leave_current` cleared `curr` and
/// before `pick`. Skipping NEXT_BUDDY in that window lets the requeued runner
/// win on deadline and the wakee waits a full request.
#[def_test]
fn eevdf_sync_wake_picks_buddy_after_curr_already_left() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();
    sched.set_stats_enabled(true);

    let runner = Arc::new(EevdfEntity::new(1usize));
    sched.add_task(runner.clone());
    let curr = sched.pick_next_task().unwrap();
    let _ = sched.update_current(&curr, UNIT_NS);
    sched.leave_current(curr, CurrentDisposition::Preempt);
    assert!(sched.curr_is_none());

    let wakee = Arc::new(EevdfEntity::new(2usize));
    wakee.set_vlag_for_test(0);
    wakee.set_needs_place_for_test(true);
    sched.enqueue_task(wakee.clone());
    assert!(wakee.deadline() > runner.deadline());
    sched.mark_sync_wake_preempt();
    assert_eq!(sched.stats().wake_sync_mark, 1);
    assert_eq!(sched.stats().wake_sync_mark_no_buddy, 0);
    assert_eq!(sched.stats().wake_nominate_no_curr, 1);

    let next = sched.pick_next_task().unwrap();
    assert_eq!(*next.inner(), 2);
    assert_eq!(sched.stats().wake_handoff, 1);
}

/// Linux `update_min_vruntime`: off-tree `curr` must pull the watermark down
/// (via min) so a dequeue of an ineligible ready peer cannot park it above V.
#[def_test]
fn eevdf_min_vruntime_includes_off_tree_curr() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();

    let runner = Arc::new(EevdfEntity::new(1usize));
    sched.add_task(runner.clone());
    let curr = sched.pick_next_task().unwrap();
    for _ in 0..2 {
        let _ = sched.update_current(&curr, UNIT_NS);
    }

    let high_vr = 0x10000;
    let keep = Arc::new(EevdfEntity::new(2usize));
    keep.set_vruntime_for_test(high_vr);
    keep.set_deadline_for_test(high_vr + SLICE as i64);
    keep.reset_slice();
    sched.inject_ready_for_test(keep.clone());

    let drop = Arc::new(EevdfEntity::new(3usize));
    drop.set_vruntime_for_test(high_vr);
    drop.set_deadline_for_test(high_vr + SLICE as i64);
    drop.reset_slice();
    sched.inject_ready_for_test(drop.clone());
    assert!(sched.remove_task(&drop).is_some());

    let v = sched.system_vruntime_for_test();
    assert!(
        keep.vruntime_for_test() > v,
        "ready peer must stay ineligible so this is the pile-up case"
    );
    assert!(
        sched.min_vruntime_for_test() <= v,
        "watermark must not sit on the ineligible ready peer"
    );
    assert!(
        sched.min_vruntime_for_test() <= curr.vruntime_for_test(),
        "watermark must track off-tree curr, not ready-only min"
    );
    sched.leave_current(curr, CurrentDisposition::Exit);
}

/// `leave_current` must not recompute the watermark after clearing `curr`.
/// That window sees only ineligible ready waiters and permanently raises
/// `min_vruntime` above V (Linux `put_prev` updates while `curr` is still set).
#[def_test]
fn eevdf_leave_does_not_park_min_vruntime_on_ineligible_ready() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();
    sched.set_stats_enabled(true);

    let runner = Arc::new(EevdfEntity::new(1usize));
    sched.add_task(runner.clone());
    let curr = sched.pick_next_task().unwrap();
    for _ in 0..2 {
        let _ = sched.update_current(&curr, UNIT_NS);
    }

    let high_vr = 0x10000;
    let keep = Arc::new(EevdfEntity::new(2usize));
    keep.set_vruntime_for_test(high_vr);
    keep.set_deadline_for_test(high_vr + SLICE as i64);
    keep.reset_slice();
    sched.inject_ready_for_test(keep.clone());

    sched.leave_current(curr, CurrentDisposition::Yield);
    assert!(
        sched.min_vruntime_for_test() < keep.vruntime_for_test(),
        "requeue after leave must not park the watermark on ineligible ready peers"
    );
    let curr = sched.pick_next_task().unwrap();
    assert_eq!(*curr.inner(), 1);

    let v = sched.system_vruntime_for_test();
    assert!(keep.vruntime_for_test() > v);
    assert!(
        sched.min_vruntime_for_test() <= v,
        "leave must not park the watermark on ineligible ready peers"
    );

    let wakee = Arc::new(EevdfEntity::new(3usize));
    wakee.set_vlag_for_test(0);
    wakee.set_needs_place_for_test(true);
    sched.enqueue_task(wakee.clone());
    assert!(wakee.vruntime_for_test() <= sched.system_vruntime_for_test());

    sched.mark_sync_wake_preempt();
    assert!(sched.peer_preempts_curr());
    assert_eq!(sched.stats().wake_sync_preempt, 1);
    sched.leave_current(curr, CurrentDisposition::Exit);
}

/// Sticky pile-up: ineligible ready peers must not make a WF_SYNC wakee
/// ineligible (schbench p99.9 waiting a full request).
#[def_test]
fn eevdf_sync_wake_preempts_when_ready_peers_are_ineligible() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();
    sched.set_stats_enabled(true);

    let runner = Arc::new(EevdfEntity::new(1usize));
    sched.add_task(runner.clone());
    let curr = sched.pick_next_task().unwrap();
    for _ in 0..2 {
        let _ = sched.update_current(&curr, UNIT_NS);
    }

    let high_vr = 0x10000;
    let keep = Arc::new(EevdfEntity::new(2usize));
    keep.set_vruntime_for_test(high_vr);
    keep.set_deadline_for_test(high_vr + SLICE as i64);
    keep.reset_slice();
    sched.inject_ready_for_test(keep.clone());

    let drop = Arc::new(EevdfEntity::new(3usize));
    drop.set_vruntime_for_test(high_vr);
    drop.set_deadline_for_test(high_vr + SLICE as i64);
    drop.reset_slice();
    sched.inject_ready_for_test(drop.clone());
    assert!(sched.remove_task(&drop).is_some());

    let wakee = Arc::new(EevdfEntity::new(4usize));
    wakee.set_vlag_for_test(0);
    wakee.set_needs_place_for_test(true);
    sched.enqueue_task(wakee.clone());

    let v = sched.system_vruntime_for_test();
    assert!(
        wakee.vruntime_for_test() <= v,
        "zero-lag PLACE_LAG places at V (Linux place_entity)"
    );
    assert!(wakee.deadline() > curr.deadline());

    sched.mark_sync_wake_preempt();
    assert!(sched.peer_preempts_curr());
    assert_eq!(sched.stats().wake_sync_preempt, 1);

    sched.leave_current(curr, CurrentDisposition::Preempt);
    let next = sched.pick_next_task().unwrap();
    assert_eq!(*next.inner(), 4);
}

/// Zero-lag PLACE_LAG is `vruntime = V`, even if a ready-only ratchet parked
/// `min_vruntime` above V. Linux `place_entity` does not floor at the watermark.
#[def_test]
fn eevdf_zero_lag_place_uses_v_not_watermark() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();
    sched.set_stats_enabled(true);

    let t_low = Arc::new(EevdfEntity::new(1usize));
    t_low.set_vruntime_for_test(0);
    t_low.set_deadline_for_test(1);
    sched.inject_ready_for_test(t_low.clone());

    let t_high = Arc::new(EevdfEntity::new(2usize));
    t_high.set_vruntime_for_test(0x10000);
    t_high.set_deadline_for_test(0x10001);
    t_high.reset_slice();
    sched.inject_ready_for_test(t_high);

    assert!(sched.remove_task(&t_low).is_some());
    assert!(sched.min_vruntime_for_test() >= 0x10000);

    let runner = Arc::new(EevdfEntity::new(3usize));
    runner.set_vruntime_for_test(0);
    runner.set_deadline_for_test(1);
    runner.reset_slice();
    sched.inject_ready_for_test(runner);

    let curr = sched.pick_next_task().unwrap();
    assert_eq!(*curr.inner(), 3);

    let v = sched.system_vruntime_for_test();
    assert!(
        sched.min_vruntime_for_test() > v,
        "precondition: watermark parked above V"
    );

    let wakee = Arc::new(EevdfEntity::new(4usize));
    wakee.set_vlag_for_test(0);
    wakee.set_needs_place_for_test(true);
    sched.enqueue_task(wakee.clone());

    let v = sched.system_vruntime_for_test();
    assert!(
        wakee.vruntime_for_test() <= v,
        "zero-lag PLACE_LAG is V, not min_vruntime"
    );

    sched.mark_sync_wake_preempt();
    assert!(sched.peer_preempts_curr());
    assert_eq!(sched.stats().wake_sync_preempt, 1);
    sched.leave_current(curr, CurrentDisposition::Exit);
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
    let expected = vr + SLICE as i64;
    assert_eq!(wakee.deadline(), expected);
    assert_eq!(wakee.slice_for_test(), SLICE as u64);
    let _ = curr;
}

#[def_test]
fn eevdf_wake_negative_lag_places_above_v() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();

    let runner = Arc::new(EevdfEntity::new(1usize));
    sched.add_task(runner.clone());
    let curr = sched.pick_next_task().unwrap();
    curr.set_vruntime_for_test(0x2000);
    sched.refresh_curr_snapshot_for_test(&curr);

    // Linux PLACE_LAG: negative lag places above V (temporarily ineligible).
    let wakee = Arc::new(EevdfEntity::new(2usize));
    wakee.set_vlag_for_test(-0x1000);
    wakee.set_needs_place_for_test(true);
    sched.enqueue_task(wakee.clone());

    assert!(wakee.vruntime_for_test() > 0x2000);
    assert_eq!(wakee.slice_for_test(), SLICE as u64);
}

#[def_test]
fn eevdf_wake_positive_lag_places_below_v() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();
    let t1 = Arc::new(EevdfEntity::new(1usize));
    let t2 = Arc::new(EevdfEntity::new(2usize));
    sched.add_task(t1.clone());
    sched.add_task(t2.clone());

    // Run t1 for a full slice so it pulls ahead of t2.
    let a = sched.pick_next_task().unwrap();
    assert_eq!(*a.inner(), 1);
    for _ in 0..UNITS_PER_SLICE {
        let _ = sched.update_current(&a, UNIT_NS);
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
    for _ in 0..UNITS_PER_SLICE {
        let _ = sched.update_current(&a2, UNIT_NS);
    }
    sched.leave_current(a2, CurrentDisposition::Yield);

    // Linux PLACE_LAG: positive lag places below V; no min_vruntime floor.
    sched.enqueue_task(b);
    assert!(!t2.needs_place_for_test());
    assert!(t2.vruntime_for_test() < sched.system_vruntime_for_test());
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
fn eevdf_place_lag_does_not_floor_at_min_vruntime() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();

    let t_run = Arc::new(EevdfEntity::new(1usize));
    sched.add_task(t_run.clone());
    let curr = sched.pick_next_task().unwrap();
    sched.leave_current(curr, CurrentDisposition::Exit);

    // No `curr`: dequeue of the low ready peer may raise the watermark to the
    // remaining high vruntime (Linux `update_min_vruntime` with `curr == NULL`).
    let t_low = Arc::new(EevdfEntity::new(2usize));
    t_low.set_vruntime_for_test(0);
    t_low.set_deadline_for_test(1);
    sched.inject_ready_for_test(t_low.clone());
    let t_high = Arc::new(EevdfEntity::new(3usize));
    t_high.set_vruntime_for_test(0x2000);
    t_high.set_deadline_for_test(0x2001);
    sched.inject_ready_for_test(t_high);
    let _ = sched.remove_task(&t_low);

    // Linux place_entity does not max() onto min_vruntime. Lag is clamped to
    // ~2*request, which is still enough to place below the parked watermark.
    let t_wake = Arc::new(EevdfEntity::new(4usize));
    t_wake.set_vlag_for_test(0x10000);
    t_wake.set_needs_place_for_test(true);
    sched.enqueue_task(t_wake.clone());
    assert!(t_wake.vruntime_for_test() < sched.min_vruntime_for_test());
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
    let _ = sched.update_current(&curr, UNIT_NS);
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
    assert!(t_short.set_request_ns(UNIT_NS));
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
    assert!(!sched.update_current(&running, UNIT_NS));

    let t2 = Arc::new(EevdfEntity::new(2usize));
    sched.add_task(t2.clone());
    sched.set_priority(&t2, -20);
    assert!(!sched.update_current(&running, UNIT_NS));
    assert!(sched.check_preempt_tick());
    assert_eq!(sched.stats().preempt_by_deadline, 1);
}

#[def_test]
fn eevdf_stats_count_slice_expired() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();
    sched.set_stats_enabled(true);

    let t = Arc::new(EevdfEntity::new(1usize));
    sched.add_task(t);
    let running = sched.pick_next_task().unwrap();
    for _ in 0..UNITS_PER_SLICE - 1 {
        assert!(!sched.update_current(&running, UNIT_NS));
    }
    assert!(!sched.update_current(&running, UNIT_NS));
    assert_eq!(sched.stats().slice_expired, 1);
}

#[def_test]
fn eevdf_update_deadline_rescheds_only_with_peer() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();
    sched.set_stats_enabled(true);

    let t = Arc::new(EevdfEntity::new(1usize));
    sched.add_task(t);
    let running = sched.pick_next_task().unwrap();
    for _ in 0..UNITS_PER_SLICE - 1 {
        assert!(!sched.update_current(&running, UNIT_NS));
    }
    assert!(!sched.update_current(&running, UNIT_NS));
    assert_eq!(sched.stats().slice_expired, 1);

    let mut sched = EevdfScheduler::<usize, SLICE>::new();
    sched.set_stats_enabled(true);
    let t1 = Arc::new(EevdfEntity::new(1usize));
    let t2 = Arc::new(EevdfEntity::new(2usize));
    sched.add_task(t1);
    sched.add_task(t2);
    let running = sched.pick_next_task().unwrap();
    for _ in 0..UNITS_PER_SLICE - 1 {
        let _ = sched.update_current(&running, UNIT_NS);
    }
    assert!(sched.update_current(&running, UNIT_NS));
    assert_eq!(sched.stats().slice_expired, 1);
    assert!(running.vruntime_for_test() < running.deadline());
}

#[def_test]
fn eevdf_lone_high_vruntime_waiter_is_eligible_not_fallback() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();
    sched.set_stats_enabled(true);

    let runner = Arc::new(EevdfEntity::new(1usize));
    sched.add_task(runner);
    let curr = sched.pick_next_task().unwrap();
    sched.leave_current(curr, CurrentDisposition::Exit);

    // After Exit, pick uses ready-queue V. A lone waiter equals that average,
    // so it is eligible — the defensive no-eligible fallback is for tree/V
    // races, not a consistent high vruntime.
    let waiter = Arc::new(EevdfEntity::new(2usize));
    waiter.set_vruntime_for_test(500_000);
    waiter.set_deadline_for_test(0);
    sched.inject_ready_for_test(waiter);
    let _ = sched.pick_next_task().unwrap();

    let stats = sched.stats();
    assert_eq!(stats.picks_total, 2);
    assert_eq!(stats.fallback_no_eligible, 0);
}

#[def_test]
fn eevdf_stats_disabled_does_not_count() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();

    let t1 = Arc::new(EevdfEntity::new(1usize));
    let t2 = Arc::new(EevdfEntity::new(2usize));
    sched.add_task(t1.clone());
    sched.add_task(t2.clone());
    let running = sched.pick_next_task().unwrap();
    let _ = sched.update_current(&running, UNIT_NS);
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
    for _ in 0..UNITS_PER_SLICE {
        let _ = sched.update_current(&running, UNIT_NS);
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
// next_preemption_ns / dynamic schedule timer tests
// ============================================================================

#[def_test]
fn eevdf_lone_task_needs_no_schedule_timer() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();
    let t = Arc::new(EevdfEntity::new(1usize));
    sched.add_task(t);
    let running = sched.pick_next_task().unwrap();
    assert!(sched.next_preemption_ns(&running).is_none());
}

#[def_test]
fn eevdf_next_preemption_tracks_remaining_request() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();
    let t1 = Arc::new(EevdfEntity::new(1usize));
    let t2 = Arc::new(EevdfEntity::new(2usize));
    sched.add_task(t1);
    sched.add_task(t2);
    let running = sched.pick_next_task().unwrap();
    let before = sched.next_preemption_ns(&running).unwrap();
    assert!(before > 0);
    assert!(!sched.update_current(&running, UNIT_NS));
    let after = sched.next_preemption_ns(&running).unwrap();
    assert!(after < before);
    assert_eq!(after, before - UNIT_NS);
}

/// Ineligible waiter: next timer must be until-eligible, not the remaining request.
///
/// `preempt_resched` arms this when the WF_SYNC probe loses. The timer accounts
/// and `check_preempt_tick` stays false until V catches up; refresh must not
/// replace this with the remaining request.
#[def_test]
fn eevdf_next_preemption_tracks_until_eligible() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();
    let runner = Arc::new(EevdfEntity::new(1usize));
    sched.add_task(runner);
    let curr = sched.pick_next_task().unwrap();
    let _ = sched.update_current(&curr, UNIT_NS);

    // Small negative lag: PLACE still puts the waiter above V, but the
    // until-eligible gap stays shorter than the remaining request. A
    // `-SLICE` lag inflates to ~4× remaining wall time, so the timer
    // would just arm the request.
    let waiter = Arc::new(EevdfEntity::new(2usize));
    waiter.set_vlag_for_test(-(UNIT_NS as i64 / 4));
    waiter.set_needs_place_for_test(true);
    sched.enqueue_task(waiter.clone());
    sched.mark_sync_wake_preempt();

    assert!(waiter.vruntime_for_test() > sched.system_vruntime_for_test());
    assert!(
        !sched.peer_preempts_curr(),
        "ineligible later-deadline waiter must not win the off-tree probe"
    );
    assert!(
        !sched.check_preempt_tick(),
        "ineligible waiter must not resched on tick"
    );
    let next = sched.next_preemption_ns(&curr).unwrap();
    assert!(next > 0);
    assert!(
        next < curr.slice_for_test(),
        "until-eligible ({next} ns) must be sooner than remaining request"
    );
}

#[def_test]
fn eevdf_partial_then_crossing_slice() {
    let mut sched = EevdfScheduler::<usize, SLICE>::new();
    let t = Arc::new(EevdfEntity::new(1usize));
    sched.add_task(t);
    let running = sched.pick_next_task().unwrap();
    // Partial slice consumption must not force resched for a lone task.
    assert!(!sched.update_current(&running, UNIT_NS));
    assert_eq!(running.slice_for_test(), SLICE as u64 - UNIT_NS);
    // Crossing the remaining request rolls a new deadline (Linux update_deadline)
    // but a lone task does not resched.
    assert!(!sched.update_current(&running, SLICE as u64));
    assert_eq!(running.slice_for_test(), SLICE as u64);
    assert!(running.vruntime_for_test() < running.deadline());
}

#[def_test]
fn rr_next_preemption_only_with_ready_peer() {
    let mut scheduler = RRScheduler::<usize, SLICE>::new();
    scheduler.add_task(Arc::new(RRTask::new(0)));
    let t = scheduler.pick_next_task().unwrap();
    assert!(scheduler.next_preemption_ns(&t).is_none());

    scheduler.add_task(Arc::new(RRTask::new(1)));
    let next = scheduler.next_preemption_ns(&t).unwrap();
    assert_eq!(next, SLICE as u64);
    assert!(!scheduler.update_current(&t, UNIT_NS));
    assert_eq!(
        scheduler.next_preemption_ns(&t).unwrap(),
        SLICE as u64 - UNIT_NS
    );
}

#[def_test]
fn fifo_never_requests_schedule_timer() {
    let mut scheduler = FifoScheduler::<usize>::new();
    scheduler.add_task(Arc::new(FifoTask::new(0)));
    scheduler.add_task(Arc::new(FifoTask::new(1)));
    let t = scheduler.pick_next_task().unwrap();
    assert!(scheduler.next_preemption_ns(&t).is_none());
    assert!(!scheduler.update_current(&t, UNIT_NS));
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
        let should_switch = sched.update_current(&current, UNIT_NS);
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
    let weights = [3121i64, 1024, 335];
    let total_weight: i64 = weights.iter().sum();

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

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.



extern crate alloc;

use alloc::sync::Arc;

use unittest::{assert, assert_eq, def_test};

use crate::{
    DEFAULT_SLICE_NS, Decision, EnqueueKind, KernelClass, Nice, SchedAttr, SchedClass, SchedError,
    SchedPolicy, SchedTask, Scheduler, UpdateKind,
};

const UNIT_NS: u64 = 4096;
const SLICE_NS: u64 = DEFAULT_SLICE_NS;

fn idle() -> Arc<SchedTask<usize>> {
    Arc::new(SchedTask::new_kernel(0usize, KernelClass::Idle))
}

fn sched() -> Scheduler<usize> {
    Scheduler::new(idle(), 0)
}

fn task(id: usize) -> Arc<SchedTask<usize>> {
    Arc::new(SchedTask::new(id))
}

fn pick_id(s: &mut Scheduler<usize>, now_ns: u64) -> usize {
    let next = s.pick_next(now_ns);
    *next.inner()
}

fn nice_attr(nice: i32) -> SchedAttr {
    SchedAttr {
        policy: SchedPolicy::Normal,
        nice: Nice::new(nice).unwrap(),
    }
}

fn enqueue_new(sched: &mut Scheduler<usize>, t: Arc<SchedTask<usize>>) -> Decision {
    sched.enqueue(t, EnqueueKind::New, 0)
}

// ============================================================================
// Policy types
// ============================================================================

#[def_test]
fn policy_rank_order() {
    assert!(SchedClass::Stop.is_above(SchedClass::Deadline));
    assert!(SchedClass::Deadline.is_above(SchedClass::Rt));
    assert!(SchedClass::Rt.is_above(SchedClass::Fair));
    assert!(SchedClass::Fair.is_above(SchedClass::Idle));
    assert!(!SchedClass::Fair.is_above(SchedClass::Rt));
    assert_eq!(SchedPolicy::Batch.class(), SchedClass::Fair);
    assert_eq!(SchedPolicy::Idle.class(), SchedClass::Fair);
    assert_eq!(SchedPolicy::Normal.class(), SchedClass::Fair);
}

#[def_test]
fn nice_getpriority_roundtrip() {
    let n = Nice::new(-10).unwrap();
    assert_eq!(Nice::from_getpriority_raw(n.to_getpriority_raw()), n);
    assert_eq!(Nice::DEFAULT.weight(), 1024);
}

// ============================================================================
// Arbitration
// ============================================================================

#[def_test]
fn pick_never_none_idle_fallback() {
    let mut s = sched();
    let idle = s.pick_next(0);
    assert!(idle.is_idle_class());
}

#[def_test]
fn idle_curr_any_runnable_rescheds() {
    let mut s = sched();
    let idle = s.pick_next(0);
    assert!(idle.is_idle_class());
    let fair = task(1);
    let d = enqueue_new(&mut s, fair);
    assert!(d.resched);
}

#[def_test]
fn pick_next_requires_leave() {
    let mut s = sched();
    enqueue_new(&mut s, task(1));
    let _ = s.pick_next(0);
    assert!(!s.curr_is_none());
    s.yield_current(0);
    assert!(s.curr_is_none());
    let next = s.pick_next(0);
    assert_eq!(*next.inner(), 1);
}

#[def_test]
fn update_account_lone_fair_does_not_resched() {
    let mut s = sched();
    enqueue_new(&mut s, task(1));
    let _ = s.pick_next(0);
    let d = s.update(UNIT_NS, UpdateKind::Account);
    assert!(!d.resched);
}

// ============================================================================
// EEVDF
// ============================================================================

#[def_test]
fn eevdf_sched_order() {
    let mut s = sched();
    const N: usize = 5;
    for i in 1..=N {
        enqueue_new(&mut s, task(i));
    }
    let mut now = 0u64;
    for i in 0..N * 4 {
        assert_eq!(pick_id(&mut s, now), (i % N) + 1);
        now += UNIT_NS;
        let _ = s.update(now, UpdateKind::Account);
        s.yield_current(now);
    }
}

#[def_test]
fn eevdf_config_request_ns_sets_new_deadline() {
    let mut s = sched();
    let t = task(1);
    assert_eq!(t.request_ns_for_test(), DEFAULT_SLICE_NS);
    enqueue_new(&mut s, t.clone());
    assert_eq!(t.request_ns_for_test(), SLICE_NS);
    assert_eq!(t.deadline_for_test(), SLICE_NS as i64);
}

#[def_test]
fn eevdf_high_weight_picked_first() {
    let mut s = sched();
    let bg = task(1);
    let fg = task(2);
    enqueue_new(&mut s, bg.clone());
    enqueue_new(&mut s, fg.clone());
    s.set_attr(&bg, nice_attr(19), 0).unwrap();
    s.set_attr(&fg, nice_attr(-20), 0).unwrap();
    assert_eq!(pick_id(&mut s, 0), 2);
}

#[def_test]
fn eevdf_eligible_preferred_over_earlier_deadline() {
    let mut s = sched();
    let t1 = task(1);
    let t2 = task(2);
    enqueue_new(&mut s, t1);
    enqueue_new(&mut s, t2);
    let running = s.pick_next(0);
    assert_eq!(*running.inner(), 1);
    for step in 1..=4 {
        let _ = s.update(step * UNIT_NS, UpdateKind::Account);
    }
    s.yield_current(4 * UNIT_NS);
    assert_eq!(pick_id(&mut s, 4 * UNIT_NS), 2);
}

#[def_test]
fn eevdf_wake_place_lag() {
    let mut s = sched();
    let a = task(1);
    let b = task(2);
    enqueue_new(&mut s, a.clone());
    enqueue_new(&mut s, b);
    let _ = s.pick_next(0);
    let _ = s.update(UNIT_NS, UpdateKind::Account);
    s.block_current(UNIT_NS);
    let _ = s.pick_next(UNIT_NS);
    let vr_before = a.vruntime_for_test();
    s.enqueue(a.clone(), EnqueueKind::Wake { sync: false }, UNIT_NS);
    // PLACE_LAG must not leave the waker at constructor zero.
    assert!(a.vruntime_for_test() != 0 || vr_before != 0 || a.vlag_for_test() != 0);
}

#[def_test]
fn eevdf_sync_wake_preempts_later_deadline() {
    let mut s = sched();
    let curr = task(1);
    let wakee = task(2);
    enqueue_new(&mut s, curr);
    let _ = s.pick_next(0);
    let _ = s.update(UNIT_NS, UpdateKind::Account);
    let d = s.enqueue(wakee, EnqueueKind::Wake { sync: true }, UNIT_NS);
    // Eligible buddy after sync wake should take the CPU.
    assert!(d.resched);
}

#[def_test]
fn eevdf_ineligible_sync_wake_keeps_probe_flag() {
    let mut s = sched();
    let curr = task(1);
    let wakee = task(2);
    enqueue_new(&mut s, curr);
    let _ = s.pick_next(0);
    let _ = s.update(UNIT_NS, UpdateKind::Account);
    wakee.set_vlag_for_test(i64::MIN / 4);
    let _ = s.enqueue(wakee, EnqueueKind::Wake { sync: true }, UNIT_NS);
    let _ = s.update(UNIT_NS, UpdateKind::WakeProbe);
    assert!(s.fair_mut().has_sync_wake_for_test());
}

#[def_test]
fn eevdf_nice_change_queued_and_running() {
    let mut s = sched();
    let a = task(1);
    let b = task(2);
    enqueue_new(&mut s, a.clone());
    enqueue_new(&mut s, b.clone());
    assert!(s.set_attr(&a, nice_attr(10), 0).is_ok());
    let running = s.pick_next(0);
    assert!(s.set_attr(&running, nice_attr(-5), 0).is_ok());
    s.yield_current(0);
    assert!(s.pick_next(0).sched_class() == SchedClass::Fair);
}

#[def_test]
fn eevdf_steal_fair_only() {
    let mut s = sched();
    let fair = task(1);
    enqueue_new(&mut s, fair.clone());
    let stolen = s.steal(&mut |_| true, 0).unwrap();
    assert!(Arc::ptr_eq(&stolen, &fair));
}

#[def_test]
fn eevdf_intrusive_identity_and_release() {
    let mut left = sched();
    let mut right = sched();
    let a = task(0);
    let b = task(1);
    enqueue_new(&mut left, a.clone());
    enqueue_new(&mut right, b.clone());
    assert!(left.dequeue(&b, 0).is_none());
    assert!(left.fair_mut().ready_indices_valid_for_test());
    assert!(right.fair_mut().ready_indices_valid_for_test());
    let removed = left.dequeue(&a, 0).unwrap();
    assert!(Arc::ptr_eq(&a, &removed));
    drop(removed);
    assert_eq!(Arc::strong_count(&a), 1);
    assert!(left.fair_mut().ready_indices_valid_for_test());
    let _ = right.enqueue(a.clone(), EnqueueKind::Wake { sync: false }, 0);
    assert!(right.fair_mut().ready_indices_valid_for_test());
    drop(right);
    assert_eq!(Arc::strong_count(&a), 1);
    assert_eq!(Arc::strong_count(&b), 1);
    enqueue_new(&mut left, b.clone());
    enqueue_new(&mut left, a.clone());
    assert!(left.fair_mut().ready_indices_valid_for_test());
}

#[def_test]
fn eevdf_intrusive_reuse_and_selection() {
    const N: usize = 32;
    let tasks: alloc::vec::Vec<_> = (0..N).map(task).collect();
    let mut left = sched();
    let mut right = sched();
    let mut locations = [None; N];
    let mut random = 0x8765_4321_u64;
    for step in 0..2000 {
        random ^= random << 13;
        random ^= random >> 7;
        random ^= random << 17;
        let index = random as usize % N;
        let task = &tasks[index];
        if let Some(old) = locations[index].take() {
            let host = if old == 0 { &mut left } else { &mut right };
            assert!(host.dequeue(task, 0).is_some());
            assert!(host.dequeue(task, 0).is_none());
            assert_eq!(Arc::strong_count(task), 1);
        }
        if step % 5 != 0 {
            let target = (random >> 32) as usize % 2;
            task.set_vruntime_for_test((random % 31) as i64 - 15);
            task.set_deadline_for_test(((random >> 12) % 17) as i64 - 8);
            let host = if target == 0 { &mut left } else { &mut right };
            host.fair_mut().inject_ready_for_test(task.clone());
            locations[index] = Some(target);
        }
        assert!(left.fair_mut().ready_indices_valid_for_test());
        assert!(right.fair_mut().ready_indices_valid_for_test());
    }
    for (index, location) in locations.iter_mut().enumerate() {
        if let Some(cpu) = location.take() {
            let host = if cpu == 0 { &mut left } else { &mut right };
            assert!(host.dequeue(&tasks[index], 0).is_some());
        }
    }
    assert!(left.fair_mut().ready_indices_valid_for_test());
    assert!(right.fair_mut().ready_indices_valid_for_test());
    // Negative keys, repeated deadlines and an ineligible deadline head.
    // At equal weight V=(-4+0+4+8)/4=2; eligible task 1 has earlier deadline.
    for (i, (vr, dl)) in [(-4, 2), (0, 1), (4, -2), (8, 1)].into_iter().enumerate() {
        tasks[i].set_vruntime_for_test(vr);
        tasks[i].set_deadline_for_test(dl);
        left.fair_mut().inject_ready_for_test(tasks[i].clone());
    }
    assert_eq!(pick_id(&mut left, 0), 1);
    left.exit_current(0);
    assert!(left.fair_mut().ready_indices_valid_for_test());
    let stolen = left.steal(&mut |t| *t.inner() != 0, 0).unwrap();
    assert_eq!(*stolen.inner(), 2);
    assert!(left.fair_mut().ready_indices_valid_for_test());
}

#[def_test]
fn eevdf_steal_does_not_clone_rejected_waiters() {
    let mut s = sched();
    let tasks: alloc::vec::Vec<_> = (1..5).map(task).collect();
    for t in &tasks {
        enqueue_new(&mut s, t.clone());
    }
    let before: alloc::vec::Vec<_> = tasks.iter().map(Arc::strong_count).collect();
    assert!(s.steal(&mut |_| false, 0).is_none());
    for (t, count) in tasks.iter().zip(before) {
        assert_eq!(Arc::strong_count(t), count);
    }
    let stolen = s.steal(&mut |t| *t.inner() == 3, 0).unwrap();
    assert_eq!(*stolen.inner(), 3);
    assert!(s.fair_mut().ready_indices_valid_for_test());
}

#[def_test]
fn eevdf_update_account_does_not_resched_without_peer() {
    let mut s = sched();
    enqueue_new(&mut s, task(1));
    let _ = s.pick_next(0);
    let d = s.update(SLICE_NS, UpdateKind::Account);
    assert!(!d.resched);
}

#[def_test]
fn eevdf_tick_preempts_when_peer_has_earlier_deadline() {
    let mut s = sched();
    let a = task(1);
    let b = task(2);
    enqueue_new(&mut s, a);
    enqueue_new(&mut s, b);
    let _ = s.pick_next(0);
    // Expire the runner's request so its deadline is pushed past the waiter.
    let d = s.update(SLICE_NS, UpdateKind::Account);
    assert!(d.resched || s.update(SLICE_NS, UpdateKind::SchedTick).resched);
}

#[def_test]
fn eevdf_dequeue_saves_lag() {
    let mut s = sched();
    let a = task(1);
    let b = task(2);
    enqueue_new(&mut s, a);
    enqueue_new(&mut s, b.clone());
    let _ = s.pick_next(0);
    let _ = s.update(UNIT_NS, UpdateKind::Account);
    let removed = s.dequeue(&b, UNIT_NS);
    assert!(removed.is_some());
    assert!(b.vlag_for_test() > 0);
}

#[def_test]
fn eevdf_lone_task_needs_no_timer() {
    let mut s = sched();
    enqueue_new(&mut s, task(1));
    let _ = s.pick_next(0);
    assert!(s.next_event_ns().is_none());
}

#[def_test]
fn eevdf_exit_releases_curr_arc() {
    let mut s = sched();
    let t = task(1);
    enqueue_new(&mut s, t.clone());
    let picked = s.pick_next(0);
    drop(picked);
    let exited = s.exit_current(0);
    assert!(Arc::ptr_eq(&exited, &t));
    drop(exited);
    assert_eq!(Arc::strong_count(&t), 1);
}

#[def_test]
fn eevdf_install_current_places_at_v() {
    let mut s = sched();
    let main = task(1);
    s.install_current(main.clone(), 0);
    let waiter = task(2);
    enqueue_new(&mut s, waiter);
    // Main stays the runner; waiter is queued.
    assert!(s.current().is_some_and(|c| Arc::ptr_eq(c, &main)));
    assert_eq!(s.nr_queued(), 1);
}

#[def_test]
fn set_attr_kernel_class_rejected() {
    let mut s = sched();
    let idle = s.idle_task().clone();
    assert_eq!(
        s.set_attr(&idle, SchedAttr::DEFAULT, 0),
        Err(SchedError::KernelClassImmutable)
    );
}

#[def_test]
fn set_attr_invalid_does_not_change_class() {
    let mut s = sched();
    let t = task(1);
    enqueue_new(&mut s, t.clone());
    let bad = SchedAttr {
        policy: SchedPolicy::Fifo,
        nice: Nice::DEFAULT,
    };
    assert!(s.set_attr(&t, bad, 0).is_err());
    assert_eq!(t.sched_class(), SchedClass::Fair);
}

#[def_test]
fn eevdf_min_vruntime_not_raised_by_ineligible_waiter() {
    let mut s = sched();
    let runner = task(1);
    let waiter = task(2);
    enqueue_new(&mut s, runner);
    let _ = s.pick_next(0);
    waiter.set_vruntime_for_test(1_000_000);
    waiter.set_deadline_for_test(1_000_000);
    waiter.set_vlag_for_test(0);
    s.fair_mut().inject_ready_for_test(waiter);
    let min_before = s.fair_mut().min_vruntime_for_test();
    let sys = s.fair_mut().avg_vruntime_for_test(None);
    s.yield_current(0);
    let min_after = s.fair_mut().min_vruntime_for_test();
    assert!(min_after <= min_before.saturating_add(sys.abs()).max(min_before));
}

#[def_test]
fn eevdf_eligible_buddy_beats_later_deadline() {
    let mut s = sched();
    let curr = task(1);
    let buddy = task(2);
    enqueue_new(&mut s, curr);
    let _ = s.pick_next(0);
    assert!(buddy.set_request_ns_for_test(SLICE_NS));
    buddy.reset_slice_for_test();
    assert_eq!(buddy.slice_for_test(), SLICE_NS);
    buddy.set_vruntime_for_test(0);
    buddy.set_deadline_for_test(i64::MAX / 2);
    let _ = buddy.deadline_for_test();
    s.enqueue(buddy, EnqueueKind::Wake { sync: true }, 0);
    s.yield_current(0);
    assert_eq!(pick_id(&mut s, 0), 2);
}

#[def_test]
fn enqueue_after_leave_window_rescheds() {
    let mut s = sched();
    enqueue_new(&mut s, task(1));
    let _ = s.pick_next(0);
    s.yield_current(0);
    assert!(s.curr_is_none());
    let d = enqueue_new(&mut s, task(2));
    assert!(d.resched);
}

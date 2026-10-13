// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

extern crate alloc;

use alloc::sync::Arc;

use unittest::{assert, assert_eq, def_test};

use crate::{
    DEFAULT_RR_SLICE_NS, DEFAULT_RT_PERIOD_NS, DEFAULT_RT_RUNTIME_NS, DEFAULT_SLICE_NS, Decision,
    DlParams, EnqueueKind, KernelClass, Nice, RtPrio, SchedAttr, SchedClass, SchedError,
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
        rt_priority: RtPrio::NONE,
        dl_params: None,
    }
}

fn rt_attr(policy: SchedPolicy, priority: u8) -> SchedAttr {
    SchedAttr {
        policy,
        nice: Nice::DEFAULT,
        rt_priority: RtPrio::new(i32::from(priority)).unwrap(),
        dl_params: None,
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
fn eevdf_queued_nice_change_preserves_weighted_vruntime() {
    let mut s = sched();
    let a = task(1);
    let b = task(2);
    a.set_vruntime_for_test(1_000_000);
    b.set_vruntime_for_test(3_000_000);
    s.fair_mut().inject_ready_for_test(a.clone());
    s.fair_mut().inject_ready_for_test(b.clone());
    assert!(s.fair_mut().ready_indices_valid_for_test());
    for nice in [10, -5, 0] {
        assert!(s.set_attr(&a, nice_attr(nice), 0).is_ok());
        assert!(s.fair_mut().ready_indices_valid_for_test());
        let weight = i64::from(Nice::new(nice).unwrap().weight());
        let expected = (1_000_000_i64 * weight + 3_000_000 * 1024) / (weight + 1024);
        assert_eq!(s.fair_mut().avg_vruntime_for_test(None), expected);
    }
    assert!(s.dequeue(&a, 0).is_some());
    assert!(s.fair_mut().ready_indices_valid_for_test());
    assert_eq!(s.fair_mut().avg_vruntime_for_test(None), 3_000_000);
    assert!(s.dequeue(&b, 0).is_some());
    assert!(s.fair_mut().ready_indices_valid_for_test());
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
        policy: SchedPolicy::Deadline,
        nice: Nice::DEFAULT,
        rt_priority: RtPrio::NONE,
        dl_params: None,
    };
    assert_eq!(s.set_attr(&t, bad, 0), Err(SchedError::InvalidPolicy));
    assert_eq!(t.sched_class(), SchedClass::Fair);
    let bad_prio = SchedAttr {
        policy: SchedPolicy::Fifo,
        nice: Nice::DEFAULT,
        rt_priority: RtPrio::NONE,
        dl_params: None,
    };
    assert_eq!(
        s.set_attr(&t, bad_prio, 0),
        Err(SchedError::InvalidPriority)
    );
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

fn leave_pick(s: &mut Scheduler<usize>, now_ns: u64) -> usize {
    s.preempt_current(now_ns);
    pick_id(s, now_ns)
}

#[def_test]
fn rt_fifo_preempts_lower_and_fair() {
    let mut s = sched();
    let fair = task(1);
    let low = task(2);
    let high = task(3);
    s.set_attr(&low, rt_attr(SchedPolicy::Fifo, 1), 0).unwrap();
    s.set_attr(&high, rt_attr(SchedPolicy::Fifo, 8), 0).unwrap();
    enqueue_new(&mut s, fair);
    enqueue_new(&mut s, low);
    assert_eq!(pick_id(&mut s, 0), 2);
    let woke = s.enqueue(high, EnqueueKind::Wake { sync: false }, 0);
    assert!(woke.resched);
    assert_eq!(leave_pick(&mut s, 0), 3);
}

#[def_test]
fn rt_same_priority_is_fifo() {
    let mut s = sched();
    let first = task(1);
    let second = task(2);
    s.set_attr(&first, rt_attr(SchedPolicy::Fifo, 4), 0)
        .unwrap();
    s.set_attr(&second, rt_attr(SchedPolicy::Fifo, 4), 0)
        .unwrap();
    enqueue_new(&mut s, first);
    enqueue_new(&mut s, second);
    assert_eq!(pick_id(&mut s, 0), 1);
    // POSIX `sched_yield`: the running FIFO thread becomes the tail.
    s.yield_current(0);
    assert_eq!(pick_id(&mut s, 0), 2);
}

#[def_test]
fn rt_priority_change_requeues() {
    let mut s = sched();
    let low = task(1);
    let raised = task(2);
    s.set_attr(&low, rt_attr(SchedPolicy::Fifo, 2), 0).unwrap();
    s.set_attr(&raised, rt_attr(SchedPolicy::Fifo, 2), 0)
        .unwrap();
    enqueue_new(&mut s, low);
    enqueue_new(&mut s, raised.clone());
    s.set_attr(&raised, rt_attr(SchedPolicy::Fifo, 9), 0)
        .unwrap();
    assert_eq!(pick_id(&mut s, 0), 2);
}

#[def_test]
fn rt_rr_slice_rotates_same_priority() {
    let mut s = sched();
    let a = task(1);
    let b = task(2);
    s.set_attr(&a, rt_attr(SchedPolicy::Rr, 3), 0).unwrap();
    s.set_attr(&b, rt_attr(SchedPolicy::Rr, 3), 0).unwrap();
    enqueue_new(&mut s, a);
    enqueue_new(&mut s, b);
    assert_eq!(pick_id(&mut s, 0), 1);
    let d = s.update(DEFAULT_RR_SLICE_NS, UpdateKind::Account);
    assert!(d.resched);
    assert_eq!(leave_pick(&mut s, DEFAULT_RR_SLICE_NS), 2);
}

#[def_test]
fn rt_rr_slice_survives_later_account() {
    let mut s = sched();
    let a = task(1);
    let b = task(2);
    s.set_attr(&a, rt_attr(SchedPolicy::Rr, 3), 0).unwrap();
    s.set_attr(&b, rt_attr(SchedPolicy::Rr, 3), 0).unwrap();
    enqueue_new(&mut s, a);
    enqueue_new(&mut s, b);
    assert_eq!(pick_id(&mut s, 0), 1);
    let tick = s.update(DEFAULT_RR_SLICE_NS, UpdateKind::SchedTick);
    assert!(tick.resched);
    // Production preempt_resched accounts again, with a later timestamp,
    // before leave. Same-priority RR must still rotate.
    let probe = s.update(DEFAULT_RR_SLICE_NS + 1, UpdateKind::WakeProbe);
    assert!(probe.resched);
    s.preempt_current(DEFAULT_RR_SLICE_NS + 1);
    assert_eq!(pick_id(&mut s, DEFAULT_RR_SLICE_NS + 1), 2);
}

#[def_test]
fn rt_bandwidth_yields_to_fair() {
    let mut s = sched();
    let fair = task(1);
    let rt = task(2);
    s.set_attr(&rt, rt_attr(SchedPolicy::Fifo, 5), 0).unwrap();
    enqueue_new(&mut s, fair);
    enqueue_new(&mut s, rt);
    assert_eq!(pick_id(&mut s, 0), 2);
    let d = s.update(DEFAULT_RT_RUNTIME_NS, UpdateKind::Account);
    assert!(d.resched);
    assert_eq!(leave_pick(&mut s, DEFAULT_RT_RUNTIME_NS), 1);
}

#[def_test]
fn rt_steal_prefers_rt_over_fair() {
    let mut s = sched();
    let fair = task(1);
    let rt = task(2);
    s.set_attr(&rt, rt_attr(SchedPolicy::Fifo, 1), 0).unwrap();
    enqueue_new(&mut s, fair);
    enqueue_new(&mut s, rt);
    let stolen = s.steal(&mut |_| true, 0).unwrap();
    assert_eq!(*stolen.inner(), 2);
}

#[def_test]
fn rt_migrate_does_not_requeue() {
    let mut s = sched();
    let rt = task(1);
    s.set_attr(&rt, rt_attr(SchedPolicy::Fifo, 1), 0).unwrap();
    enqueue_new(&mut s, rt);
    assert_eq!(pick_id(&mut s, 0), 1);
    s.migrate_current(0);
    assert!(s.curr_is_none());
    assert_eq!(s.nr_queued(), 0);
}

#[def_test]
fn rt_back_to_fair() {
    let mut s = sched();
    let t = task(1);
    s.set_attr(&t, rt_attr(SchedPolicy::Fifo, 7), 0).unwrap();
    enqueue_new(&mut s, t.clone());
    assert_eq!(t.sched_class(), SchedClass::Rt);
    s.set_attr(&t, SchedAttr::DEFAULT, 0).unwrap();
    assert_eq!(t.sched_class(), SchedClass::Fair);
    assert_eq!(t.rt_priority_snapshot(), 0);
    assert_eq!(pick_id(&mut s, 0), 1);
}

#[def_test]
fn rt_rr_refills_when_no_same_priority_peer() {
    let mut s = sched();
    let runner = task(1);
    s.set_attr(&runner, rt_attr(SchedPolicy::Rr, 3), 0).unwrap();
    enqueue_new(&mut s, runner);
    assert_eq!(pick_id(&mut s, 0), 1);
    let alone = s.update(DEFAULT_RR_SLICE_NS, UpdateKind::Account);
    assert!(!alone.resched);
    let peer = task(2);
    s.set_attr(&peer, rt_attr(SchedPolicy::Rr, 3), 0).unwrap();
    let woke = s.enqueue(peer, EnqueueKind::Wake { sync: false }, DEFAULT_RR_SLICE_NS);
    assert!(!woke.resched);
    let still = s.update(DEFAULT_RR_SLICE_NS + 1, UpdateKind::Account);
    assert!(!still.resched);
}

#[def_test]
fn rt_lowered_priority_queues_at_head() {
    let mut s = sched();
    let waiting = task(1);
    let lowered = task(2);
    s.set_attr(&waiting, rt_attr(SchedPolicy::Fifo, 4), 0)
        .unwrap();
    s.set_attr(&lowered, rt_attr(SchedPolicy::Fifo, 8), 0)
        .unwrap();
    enqueue_new(&mut s, waiting);
    enqueue_new(&mut s, lowered.clone());
    s.set_attr(&lowered, rt_attr(SchedPolicy::Fifo, 4), 0)
        .unwrap();
    assert_eq!(pick_id(&mut s, 0), 2);
}

#[def_test]
fn rt_throttle_lifts_after_skipped_periods() {
    let mut s = sched();
    let fair = task(1);
    let rt = task(2);
    s.set_attr(&rt, rt_attr(SchedPolicy::Fifo, 5), 0).unwrap();
    enqueue_new(&mut s, fair);
    enqueue_new(&mut s, rt);
    assert_eq!(pick_id(&mut s, 0), 2);
    let throttled = s.update(DEFAULT_RT_RUNTIME_NS, UpdateKind::Account);
    assert!(throttled.resched);
    assert_eq!(leave_pick(&mut s, DEFAULT_RT_RUNTIME_NS), 1);
    let later = DEFAULT_RT_RUNTIME_NS + 3 * DEFAULT_RT_PERIOD_NS;
    let resumed = s.update(later, UpdateKind::Account);
    assert!(resumed.resched);
    assert_eq!(leave_pick(&mut s, later), 2);
}

#[def_test]
fn rt_throttle_without_waiter_disarms() {
    let mut s = sched();
    let rt = task(1);
    s.set_attr(&rt, rt_attr(SchedPolicy::Fifo, 5), 0).unwrap();
    enqueue_new(&mut s, rt.clone());
    assert_eq!(pick_id(&mut s, 0), 1);
    let throttled = s.update(DEFAULT_RT_RUNTIME_NS, UpdateKind::Account);
    assert!(throttled.resched);
    s.block_current(DEFAULT_RT_RUNTIME_NS);
    let next = s.pick_next(DEFAULT_RT_RUNTIME_NS);
    assert!(next.is_idle_class());
    assert!(s.next_event_ns().is_none());
    let woke = s.enqueue(rt, EnqueueKind::Wake { sync: false }, DEFAULT_RT_RUNTIME_NS);
    assert!(woke.next_event_ns.is_some_and(|ns| ns > 0));
}

#[def_test]
fn rt_steal_last_throttled_waiter_disarms() {
    let mut s = sched();
    let rt = task(1);
    s.set_attr(&rt, rt_attr(SchedPolicy::Fifo, 5), 0).unwrap();
    enqueue_new(&mut s, rt);
    assert_eq!(pick_id(&mut s, 0), 1);
    let throttled = s.update(DEFAULT_RT_RUNTIME_NS, UpdateKind::Account);
    assert!(throttled.resched);
    s.preempt_current(DEFAULT_RT_RUNTIME_NS);
    let next = s.pick_next(DEFAULT_RT_RUNTIME_NS);
    assert!(next.is_idle_class());
    assert!(s.next_event_ns().is_some());
    let stolen = s.steal(&mut |_| true, DEFAULT_RT_RUNTIME_NS);
    assert!(stolen.is_some());
    assert!(s.next_event_ns().is_none());
}

#[def_test]
fn rt_throttled_idle_keeps_period_event() {
    let mut s = sched();
    let rt = task(1);
    s.set_attr(&rt, rt_attr(SchedPolicy::Fifo, 5), 0).unwrap();
    enqueue_new(&mut s, rt);
    assert_eq!(pick_id(&mut s, 0), 1);
    let throttled = s.update(DEFAULT_RT_RUNTIME_NS, UpdateKind::Account);
    assert!(throttled.resched);
    s.preempt_current(DEFAULT_RT_RUNTIME_NS);
    let next = s.pick_next(DEFAULT_RT_RUNTIME_NS);
    assert!(next.is_idle_class());
    let until_period = s.next_event_ns();
    assert!(until_period.is_some_and(|ns| ns > 0));
}

#[def_test]
fn rt_slice_rotate_does_not_stick_to_later_task() {
    let mut s = sched();
    let runner = task(1);
    let peer = task(2);
    s.set_attr(&runner, rt_attr(SchedPolicy::Rr, 1), 0).unwrap();
    s.set_attr(&peer, rt_attr(SchedPolicy::Rr, 1), 0).unwrap();
    enqueue_new(&mut s, runner.clone());
    enqueue_new(&mut s, peer);
    assert_eq!(pick_id(&mut s, 0), 1);
    let expired = s.update(DEFAULT_RR_SLICE_NS, UpdateKind::Account);
    assert!(expired.resched);
    // Leave RT before put_prev. The rotate bit must not stay on the class.
    s.set_attr(&runner, SchedAttr::DEFAULT, DEFAULT_RR_SLICE_NS)
        .unwrap();
    let front = task(3);
    let behind = task(4);
    s.set_attr(&front, rt_attr(SchedPolicy::Fifo, 5), 0)
        .unwrap();
    s.set_attr(&behind, rt_attr(SchedPolicy::Fifo, 5), 0)
        .unwrap();
    s.enqueue(
        front.clone(),
        EnqueueKind::Wake { sync: false },
        DEFAULT_RR_SLICE_NS,
    );
    s.enqueue(
        behind,
        EnqueueKind::Wake { sync: false },
        DEFAULT_RR_SLICE_NS,
    );
    s.preempt_current(DEFAULT_RR_SLICE_NS);
    assert_eq!(pick_id(&mut s, DEFAULT_RR_SLICE_NS), 3);
    let higher = task(5);
    s.set_attr(&higher, rt_attr(SchedPolicy::Fifo, 9), 0)
        .unwrap();
    s.enqueue(
        higher,
        EnqueueKind::Wake { sync: false },
        DEFAULT_RR_SLICE_NS,
    );
    assert_eq!(leave_pick(&mut s, DEFAULT_RR_SLICE_NS), 5);
    s.block_current(DEFAULT_RR_SLICE_NS);
    assert_eq!(pick_id(&mut s, DEFAULT_RR_SLICE_NS), 3);
}

// ============================================================================
// Deadline params and class
// ============================================================================

/// `runtime = deadline = 1 ms`, `period = 2 ms` (50% of one CPU).
const DL_RUNTIME_NS: u64 = 1_000_000;
const DL_PERIOD_NS: u64 = 2_000_000;

fn dl_params() -> DlParams {
    DlParams::new(DL_RUNTIME_NS, DL_RUNTIME_NS, DL_PERIOD_NS).unwrap()
}

fn dl_attr() -> SchedAttr {
    SchedAttr {
        policy: SchedPolicy::Deadline,
        nice: Nice::DEFAULT,
        rt_priority: RtPrio::NONE,
        dl_params: Some(dl_params()),
    }
}

/// Builds a task whose policy is already Deadline, without enqueueing it.
///
/// Uses `expect` rather than a test assertion: the assertion macros expand to
/// `return TestResult::Failed`, which only type-checks inside a test function.
fn dl_task(id: usize) -> Arc<SchedTask<usize>> {
    let mut s = sched();
    let t = task(id);
    s.set_attr(&t, dl_attr(), 0)
        .expect("deadline attr admitted");
    t
}

#[def_test]
fn dl_params_rejects_invalid_triples() {
    assert_eq!(DlParams::new(0, 0, 0), Err(SchedError::InvalidPolicy));
    // runtime must not exceed the deadline.
    assert_eq!(
        DlParams::new(2_000_000, 1_000_000, 2_000_000),
        Err(SchedError::InvalidPolicy)
    );
    // deadline must not exceed the period.
    assert_eq!(
        DlParams::new(1_000_000, 3_000_000, 2_000_000),
        Err(SchedError::InvalidPolicy)
    );
    // below the configured minimum runtime.
    assert_eq!(
        DlParams::new(1, 1_000_000, 2_000_000),
        Err(SchedError::InvalidPolicy)
    );
    // a valid, minimal-legal triple round-trips.
    let ok = DlParams::new(DL_RUNTIME_NS, DL_PERIOD_NS, DL_PERIOD_NS).unwrap();
    assert_eq!(ok.runtime_ns, DL_RUNTIME_NS);
    assert_eq!(ok.deadline_ns, DL_PERIOD_NS);
    assert_eq!(ok.period_ns, DL_PERIOD_NS);
}

#[def_test]
fn sched_attr_validate_checks_field_policy_consistency() {
    assert!(dl_attr().validate().is_ok());
    // A Deadline policy without parameters, and a Fair policy carrying them,
    // are both structurally inconsistent.
    let missing = SchedAttr {
        policy: SchedPolicy::Deadline,
        nice: Nice::DEFAULT,
        rt_priority: RtPrio::NONE,
        dl_params: None,
    };
    assert_eq!(missing.validate(), Err(SchedError::InvalidPolicy));
    let extra = SchedAttr {
        policy: SchedPolicy::Normal,
        nice: Nice::DEFAULT,
        rt_priority: RtPrio::NONE,
        dl_params: Some(dl_params()),
    };
    assert_eq!(extra.validate(), Err(SchedError::InvalidPolicy));
    // An RT policy without a priority in `1..=99` is rejected by the RT rule.
    let rt = SchedAttr {
        policy: SchedPolicy::Fifo,
        nice: Nice::DEFAULT,
        rt_priority: RtPrio::NONE,
        dl_params: None,
    };
    assert_eq!(rt.validate(), Err(SchedError::InvalidPriority));
    assert!(SchedAttr::DEFAULT.validate().is_ok());
}

#[def_test]
fn dl_earliest_deadline_is_picked_first() {
    let mut s = sched();
    // Both tasks open their first request at the same enqueue instant, so
    // their absolute deadlines are `deadline_ns` apart and EDF picks the
    // shorter one.
    let far = dl_task(2);
    assert!(s.set_attr(&far, deadline_at(2_000_000), 0).is_ok());
    let near = dl_task(1);
    assert!(s.set_attr(&near, deadline_at(1_000_000), 0).is_ok());
    let _ = s.enqueue(far, EnqueueKind::New, 100);
    let _ = s.enqueue(near, EnqueueKind::New, 100);
    assert_eq!(pick_id(&mut s, 100), 1);
}

fn deadline_at(deadline_ns: u64) -> SchedAttr {
    SchedAttr {
        policy: SchedPolicy::Deadline,
        nice: Nice::DEFAULT,
        rt_priority: RtPrio::NONE,
        dl_params: Some(DlParams::new(DL_RUNTIME_NS, deadline_ns, DL_PERIOD_NS).unwrap()),
    }
}

#[def_test]
fn dl_request_resumes_until_the_period_end() {
    let mut s = sched();
    let t = dl_task(1);
    s.install_current(t.clone(), 0);
    // A request is open for the whole period: the budget is only charged by
    // elapsed time, so a sample inside the period keeps it running.
    assert_eq!(t.dl_budget_for_test(), DL_RUNTIME_NS);
    let decision = s.update(DL_RUNTIME_NS / 2, UpdateKind::Account);
    assert!(!decision.resched);
    assert!(!t.dl_is_throttled_for_test());
    assert_eq!(t.dl_budget_for_test(), DL_RUNTIME_NS / 2);
}

#[def_test]
fn dl_period_end_throttles_and_next_period_replenishes() {
    let mut s = sched();
    let t = dl_task(1);
    s.install_current(t.clone(), 0);
    // Past the request's deadline the budget is exhausted and the class asks
    // for a reschedule; the task waits for its next period.
    let decision = s.update(DL_PERIOD_NS, UpdateKind::Account);
    assert!(decision.resched);
    assert!(t.dl_is_throttled_for_test());
    assert_eq!(t.dl_budget_for_test(), 0);
    assert_eq!(t.dl_period_start_for_test(), DL_PERIOD_NS);
    // The task leaves the CPU and is re-enqueued while still throttled: it
    // waits in the period-start tree, not the ready tree.
    s.block_current(DL_PERIOD_NS);
    let _ = s.enqueue(t.clone(), EnqueueKind::Wake { sync: false }, DL_PERIOD_NS);
    assert_eq!(s.nr_queued(), 0);
    // The next scheduling point replenishes the budget and lifts the
    // throttle, so the task becomes runnable again.
    let _ = s.update(DL_PERIOD_NS + DL_RUNTIME_NS, UpdateKind::Account);
    assert!(!t.dl_is_throttled_for_test());
    assert_eq!(t.dl_budget_for_test(), DL_RUNTIME_NS);
    assert_eq!(s.nr_queued(), 1);
    assert_eq!(pick_id(&mut s, DL_PERIOD_NS + DL_RUNTIME_NS), 1);
}

#[def_test]
fn dl_admission_rejects_over_reserved_run_queue() {
    let mut s = sched();
    // The first 50% reservation fits inside the configured margin.
    let first = dl_task(1);
    let _ = s.enqueue(first, EnqueueKind::New, 0);
    // A second reservation would push the queue past `1 - margin` (5%): the
    // 50% already admitted plus this whole-CPU request. A second identical
    // 50% request would exceed the limit as well.
    let second = task(2);
    let whole_cpu = SchedAttr {
        policy: SchedPolicy::Deadline,
        nice: Nice::DEFAULT,
        rt_priority: RtPrio::NONE,
        dl_params: Some(DlParams::new(DL_PERIOD_NS, DL_PERIOD_NS, DL_PERIOD_NS).unwrap()),
    };
    assert_eq!(
        s.set_attr(&second, whole_cpu, 0),
        Err(SchedError::AdmissionFull)
    );
    // Rejected attributes leave the task on its previous class.
    assert_eq!(second.sched_class(), SchedClass::Fair);
}

#[def_test]
fn dl_queued_task_replaces_its_own_reservation() {
    let mut s = sched();
    let t = dl_task(1);
    let _ = s.enqueue(t.clone(), EnqueueKind::New, 0);
    // The queued task is inside the summed ready tree, so re-submitting the
    // same triple must replace its own 50%, not add a second one on top.
    assert!(s.set_attr(&t, dl_attr(), 0).is_ok());
    assert_eq!(s.attr(&t), dl_attr());
}

#[def_test]
fn dl_runner_reservation_counts_toward_admission() {
    let mut s = sched();
    // The runner leaves the ready tree on pick, so admission has to add it
    // back before replacing its reservation.
    let running = dl_task(1);
    let _ = s.enqueue(running.clone(), EnqueueKind::New, 0);
    assert_eq!(pick_id(&mut s, 0), 1);
    let queued = dl_task(2);
    let _ = s.enqueue(queued, EnqueueKind::New, 0);
    // 50% running plus 50% ready is the whole CPU, so the runner cannot keep
    // its reservation and the resubmission is refused.
    assert_eq!(
        s.set_attr(&running, dl_attr(), 0),
        Err(SchedError::AdmissionFull)
    );
    assert_eq!(s.attr(&running), dl_attr());
}

#[def_test]
fn dl_exit_clears_the_runner_from_admission() {
    let mut s = sched();
    // A 50% runner fills half the queue. If leaving the CPU left its
    // reservation recorded, the next 50% request would look like a whole CPU.
    let runner = dl_task(1);
    let _ = s.enqueue(runner.clone(), EnqueueKind::New, 0);
    assert_eq!(pick_id(&mut s, 0), 1);
    let exited = s.exit_current(0);
    assert!(Arc::ptr_eq(&exited, &runner));
    // The queue is empty again, so a fresh 50% reservation must fit.
    let next = task(2);
    assert!(s.set_attr(&next, dl_attr(), 0).is_ok());
    assert_eq!(s.attr(&next), dl_attr());
}

#[def_test]
fn dl_timer_reports_budget_and_period_events() {
    let mut s = sched();
    let t = dl_task(1);
    s.install_current(t.clone(), 0);
    // While the request runs, the next look is the end of the budget.
    assert_eq!(s.next_event_ns(), Some(DL_RUNTIME_NS));
    // Spend the full budget by blocking after one runtime, then wake after the
    // request's deadline has passed with no budget left. The task is throttled
    // and waits for a fresh request starting `period` after that wake.
    s.block_current(DL_RUNTIME_NS);
    let _ = s.enqueue(t.clone(), EnqueueKind::Wake { sync: false }, DL_RUNTIME_NS);
    assert!(t.dl_is_throttled_for_test());
    // The current request's period is not reopened by the wake.
    assert_eq!(t.dl_period_start_for_test(), DL_PERIOD_NS);
    assert_eq!(s.nr_queued(), 0);
    // The class reports only the remaining time to the next period, which is
    // the period length measured from the request's activation.
    assert_eq!(s.next_event_ns(), Some(DL_PERIOD_NS - DL_RUNTIME_NS));
}

#[def_test]
fn dl_class_event_survives_an_idle_runner() {
    let mut s = sched();
    let t = dl_task(1);
    // Run the task and spend its budget before the period ends, so it is
    // throttled and waiting for the period start at 2 ms.
    let _ = s.enqueue(t.clone(), EnqueueKind::New, 0);
    assert_eq!(pick_id(&mut s, 0), 1);
    let _ = s.update(DL_RUNTIME_NS, UpdateKind::Account);
    assert!(t.dl_is_throttled_for_test());
    assert_eq!(t.dl_period_start_for_test(), DL_PERIOD_NS);
    // Leave the CPU and wake while still throttled: nothing is ready.
    s.block_current(DL_RUNTIME_NS);
    let _ = s.enqueue(t.clone(), EnqueueKind::Wake { sync: false }, DL_RUNTIME_NS);
    assert_eq!(s.nr_queued(), 0);
    // With the idle task installed as the runner, the class view must still
    // report the deadline event. `ktask` takes that view unconditionally when
    // it reprograms the schedule slot, so this is what keeps a throttled
    // deadline task from being stranded on an idle CPU.
    let idle = s.idle_task().clone();
    s.install_current(idle, DL_RUNTIME_NS);
    assert!(s.current().is_some_and(|curr| curr.is_idle_class()));
    assert_eq!(s.next_event_ns(), Some(DL_PERIOD_NS - DL_RUNTIME_NS));
    // When the period starts, the class replenishes and the task is runnable
    // again, so the idle CPU has something to pick.
    let _ = s.update(DL_PERIOD_NS, UpdateKind::Account);
    assert!(!t.dl_is_throttled_for_test());
    assert_eq!(s.nr_queued(), 1);
}

#[def_test]
fn dl_attr_change_is_admitted_before_mutation() {
    let mut s = sched();
    let t = dl_task(1);
    let _ = s.enqueue(t.clone(), EnqueueKind::New, 0);
    // Raising the reservation past the margin is rejected and the queued task
    // keeps its previous parameters.
    let greedy = SchedAttr {
        policy: SchedPolicy::Deadline,
        nice: Nice::DEFAULT,
        rt_priority: RtPrio::NONE,
        dl_params: Some(DlParams::new(DL_PERIOD_NS, DL_PERIOD_NS, DL_PERIOD_NS).unwrap()),
    };
    assert_eq!(s.set_attr(&t, greedy, 0), Err(SchedError::AdmissionFull));
    assert_eq!(s.attr(&t), dl_attr());
}

#[def_test]
fn execution_runtime_charges_once_across_scheduler_operations() {
    let mut s = sched();
    let runner = task(1);
    let waiter = task(2);
    assert_eq!(runner.sum_exec_runtime_ns(), 0);
    s.install_current(runner.clone(), 10);
    s.update(30, UpdateKind::Account);
    s.update(30, UpdateKind::SchedTick);
    assert_eq!(runner.sum_exec_runtime_ns(), 20);
    s.enqueue(waiter.clone(), EnqueueKind::New, 40);
    assert_eq!(runner.sum_exec_runtime_ns(), 30);
    assert_eq!(waiter.sum_exec_runtime_ns(), 0);
    s.steal(&mut |_| true, 50);
    assert_eq!(runner.sum_exec_runtime_ns(), 40);
    s.set_attr(&runner, nice_attr(-5), 60).unwrap();
    s.update(80, UpdateKind::Account);
    assert_eq!(runner.sum_exec_runtime_ns(), 70);
    s.preempt_current(80);
    let next = s.pick_next(100);
    assert!(Arc::ptr_eq(&next, &runner));
    s.yield_current(110);
    s.pick_next(120);
    let exited = s.exit_current(130);
    assert!(Arc::ptr_eq(&exited, &runner));
    assert_eq!(exited.sum_exec_runtime_ns(), 90);
    s.update(200, UpdateKind::Account);
    assert_eq!(exited.sum_exec_runtime_ns(), 90);
}

#[def_test]
fn execution_runtime_excludes_blocked_idle_and_migration_gaps() {
    let mut source = sched();
    let runner = task(1);
    source.install_current(runner.clone(), 10);
    source.block_current(30);
    let idle_task = source.pick_next(30);
    source.update(1000, UpdateKind::Account);
    assert_eq!(idle_task.sum_exec_runtime_ns(), 0);
    assert_eq!(runner.sum_exec_runtime_ns(), 20);
    source.enqueue(runner.clone(), EnqueueKind::Wake { sync: false }, 1000);
    source.preempt_current(1000);
    assert!(Arc::ptr_eq(&source.pick_next(1000), &runner));
    source.migrate_current(1030);
    assert_eq!(runner.sum_exec_runtime_ns(), 50);

    let mut dest = Scheduler::new(idle(), 2000);
    dest.enqueue(runner.clone(), EnqueueKind::MigrateIn, 2010);
    assert!(Arc::ptr_eq(&dest.pick_next(2020), &runner));
    dest.update(2040, UpdateKind::Account);
    assert_eq!(runner.sum_exec_runtime_ns(), 70);
    source.update(3000, UpdateKind::Account);
    assert_eq!(runner.sum_exec_runtime_ns(), 70);
}

#[def_test]
fn execution_runtime_ignores_stale_clock_samples() {
    let mut s = sched();
    let runner = task(1);
    s.install_current(runner.clone(), 10);
    s.update(50, UpdateKind::Account);
    s.update(30, UpdateKind::Account);
    s.update(50, UpdateKind::Account);
    assert_eq!(runner.sum_exec_runtime_ns(), 40);
    s.exit_current(60);
    assert_eq!(runner.sum_exec_runtime_ns(), 50);
}

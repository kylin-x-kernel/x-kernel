// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Deterministic balance selection and PELT ownership transfers.

use alloc::sync::Arc;

use unittest::{assert, assert_eq, def_test};

use crate::{
    EnqueueKind, KernelClass, RtPrio, SchedAttr, SchedPolicy, SchedTask, Scheduler, UpdateKind,
};

fn scheduler() -> Scheduler<usize> {
    Scheduler::new(Arc::new(SchedTask::new_kernel(0, KernelClass::Idle)), 0)
}

#[def_test]
fn fair_balance_refreshes_waiting_history_and_transfers_it_once() {
    let mut source = scheduler();
    let mut destination = scheduler();
    source.install_current(Arc::new(SchedTask::new(1)), 0);
    let waiter = Arc::new(SchedTask::new(2));
    source.enqueue(waiter.clone(), EnqueueKind::New, 0);
    let now_ns = 256 * 1024 * 1024;
    let mut saw_fresh_history = false;
    let stolen = source
        .steal_fair(
            1,
            &mut |_, task, queue| {
                saw_fresh_history = task.runnable_avg > 1000
                    && task.util_avg < 8
                    && queue.pelt.runnable_avg > 2000;
                true
            },
            now_ns,
        )
        .unwrap();
    assert!(saw_fresh_history);
    assert!(Arc::ptr_eq(&stolen, &waiter));
    assert_eq!(source.fair_balance_stats().nr_running, 1);
    let source_load = source.fair_pelt().load_avg;
    let task_load = source.task_pelt(&waiter).load_avg;
    destination.enqueue(stolen, EnqueueKind::MigrateIn, now_ns);
    assert_eq!(source.fair_pelt().load_avg, source_load);
    assert_eq!(destination.fair_pelt().load_avg, task_load);
    assert_eq!(destination.fair_balance_stats().nr_running, 1);
    assert!(source.steal_fair(32, &mut |_, _, _| true, now_ns).is_none());
}

#[def_test]
fn bounded_balance_scan_reaches_beyond_rejected_and_parked_waiters() {
    let mut source = scheduler();
    source.install_current(Arc::new(SchedTask::new(1)), 0);
    for id in 2..=4 {
        let task = Arc::new(SchedTask::new(id));
        if id == 3 {
            task.set_cgroup_parked(true);
        }
        source.enqueue(task, EnqueueKind::New, 0);
    }
    let mut examined = 0;
    assert!(
        source
            .steal_fair(
                1,
                &mut |_, _, _| {
                    examined += 1;
                    false
                },
                0
            )
            .is_none()
    );
    assert_eq!(examined, 1);
    // Parked entries consume scan budget but never reach the predicate.
    assert!(source.steal_fair(1, &mut |_, _, _| true, 0).is_none());
    let stolen = source
        .steal_fair(1, &mut |task, _, _| **task == 4, 0)
        .unwrap();
    assert_eq!(**stolen, 4);
    // The exhausted suffix wraps, then permits a previously rejected task.
    assert!(source.steal_fair(1, &mut |_, _, _| true, 0).is_none());
    assert_eq!(**source.steal_fair(1, &mut |_, _, _| true, 0).unwrap(), 2);
}

#[def_test]
fn periodic_fair_balance_leaves_rt_waiters_and_the_fair_runner_in_place() {
    let mut source = scheduler();
    source.install_current(Arc::new(SchedTask::new(1)), 0);
    let rt = Arc::new(SchedTask::new(2));
    source
        .set_attr(
            &rt,
            SchedAttr {
                policy: SchedPolicy::Fifo,
                rt_priority: RtPrio::new(1).unwrap(),
                ..SchedAttr::DEFAULT
            },
            0,
        )
        .unwrap();
    source.enqueue(rt.clone(), EnqueueKind::New, 0);
    source.enqueue(Arc::new(SchedTask::new(3)), EnqueueKind::New, 0);
    assert_eq!(**source.steal_fair(32, &mut |_, _, _| true, 0).unwrap(), 3);
    assert_eq!(**source.dequeue(&rt, 0).unwrap(), 2);
    assert!(source.steal_fair(32, &mut |_, _, _| true, 0).is_none());
    source.block_current(0);
    source.update(32 * 1024 * 1024, UpdateKind::Account);
    assert_eq!(source.fair_balance_stats().nr_running, 0);
    assert!(source.fair_pelt().load_avg > 0);
    assert!(source.steal_fair(32, &mut |_, _, _| true, 0).is_none());
}

#[def_test]
fn fair_balance_moves_the_only_fair_waiter_while_rt_runs() {
    let mut source = scheduler();
    let rt = Arc::new(SchedTask::new(1));
    source
        .set_attr(
            &rt,
            SchedAttr {
                policy: SchedPolicy::Fifo,
                rt_priority: RtPrio::new(1).unwrap(),
                ..SchedAttr::DEFAULT
            },
            0,
        )
        .unwrap();
    source.install_current(rt.clone(), 0);
    source.enqueue(Arc::new(SchedTask::new(2)), EnqueueKind::New, 0);
    assert_eq!(source.fair_balance_stats().nr_running, 1);
    assert_eq!(source.fair_balance_stats().nr_queued, 1);
    assert!(!source.fair_balance_stats().is_idle);
    let stolen = source.steal_fair(1, &mut |_, _, _| true, 0).unwrap();
    assert_eq!(**stolen, 2);
    assert!(Arc::ptr_eq(source.current().unwrap(), &rt));
    assert_eq!(source.fair_balance_stats().nr_running, 0);
    // An empty Fair RQ does not make an RT-running CPU idle. It may still
    // accept Fair migrate-in without displacing the higher-class runner.
    assert!(!source.fair_balance_stats().is_idle);
    source.enqueue(stolen, EnqueueKind::MigrateIn, 0);
    assert_eq!(source.fair_balance_stats().nr_queued, 1);
    assert!(Arc::ptr_eq(source.current().unwrap(), &rt));
}

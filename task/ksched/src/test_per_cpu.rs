// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use unittest::{assert, assert_eq, def_test};

use super::{HasSchedulerId, PerCpuScheduler, SchedulerKind};
use crate::BaseScheduler;

#[derive(Debug)]
struct TestTask {
    id: u64,
}

impl TestTask {
    const fn new(id: u64) -> Self {
        Self { id }
    }
}

impl HasSchedulerId for TestTask {
    fn sched_id(&self) -> u64 {
        self.id
    }
}

#[def_test]
fn per_cpu_eevdf_untracked_current_is_ignored() {
    let mut sched = PerCpuScheduler::<TestTask>::new(SchedulerKind::Eevdf);
    let idle = Arc::new(TestTask::new(99));

    assert!(!sched.task_tick(&idle));
    assert!(!sched.set_priority(&idle, 5));
    sched.put_prev_task(idle, true);

    assert!(sched.pick_next_task().is_none());
}

#[def_test]
fn per_cpu_eevdf_running_priority_change_takes_effect_on_requeue() {
    let mut sched = PerCpuScheduler::<TestTask>::new(SchedulerKind::Eevdf);
    let t1 = Arc::new(TestTask::new(1));
    let t2 = Arc::new(TestTask::new(2));

    sched.add_task(t1.clone());
    sched.add_task(t2);

    let running = sched.pick_next_task().unwrap();
    assert_eq!(running.sched_id(), 1);
    assert!(sched.set_priority(&running, -20));

    sched.put_prev_task(running, false);

    let next = sched.pick_next_task().unwrap();
    assert_eq!(next.sched_id(), 1);
}

#[def_test]
fn per_cpu_eevdf_deadline_preemption_updates_stats() {
    let mut sched = PerCpuScheduler::<TestTask>::new(SchedulerKind::Eevdf);
    let t1 = Arc::new(TestTask::new(1));
    sched.add_task(t1.clone());

    let running = sched.pick_next_task().unwrap();
    assert_eq!(running.sched_id(), 1);
    assert!(!sched.task_tick(&running));

    sched.set_stats_enabled(true);

    let t2 = Arc::new(TestTask::new(2));
    sched.add_task(t2.clone());
    assert!(sched.set_priority(&t2, -20));

    assert!(sched.task_tick(&running));

    let stats = sched.stats();
    assert_eq!(stats.preempt_by_deadline, 1);
    assert_eq!(stats.slice_expired, 0);
}

#[def_test]
fn per_cpu_rr_preempted_task_stays_at_front_until_slice_expires() {
    let mut sched = PerCpuScheduler::<TestTask>::new(SchedulerKind::Rr);
    let t1 = Arc::new(TestTask::new(1));
    let t2 = Arc::new(TestTask::new(2));

    sched.add_task(t1.clone());
    sched.add_task(t2);

    let running = sched.pick_next_task().unwrap();
    assert_eq!(running.sched_id(), 1);
    assert!(!sched.task_tick(&running));
    sched.put_prev_task(running.clone(), true);

    let picked_again = sched.pick_next_task().unwrap();
    assert_eq!(picked_again.sched_id(), 1);

    for _ in 0..3 {
        assert!(!sched.task_tick(&picked_again));
    }
    assert!(sched.task_tick(&picked_again));
    sched.put_prev_task(picked_again, true);

    let next = sched.pick_next_task().unwrap();
    assert_eq!(next.sched_id(), 2);
}

#[def_test]
fn per_cpu_fifo_preempt_flag_does_not_bypass_fifo_order() {
    let mut sched = PerCpuScheduler::<TestTask>::new(SchedulerKind::Fifo);
    let t1 = Arc::new(TestTask::new(1));
    let t2 = Arc::new(TestTask::new(2));

    sched.add_task(t1.clone());
    sched.add_task(t2);

    let running = sched.pick_next_task().unwrap();
    assert_eq!(running.sched_id(), 1);
    sched.put_prev_task(running, true);

    let next = sched.pick_next_task().unwrap();
    assert_eq!(next.sched_id(), 2);
}

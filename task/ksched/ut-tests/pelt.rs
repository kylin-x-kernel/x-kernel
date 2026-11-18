// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Integer reference vectors and deterministic Fair lifecycle regressions.

use alloc::sync::Arc;

use unittest::{assert, assert_eq, def_test};

use super::{Pelt, PeltAvg, decay};
use crate::{EnqueueKind, KernelClass, Nice, RtPrio, SchedAttr, SchedTask, Scheduler, UpdateKind};

const PERIOD_NS: u64 = 1024 * 1024;

fn scheduler(now_ns: u64) -> Scheduler<usize> {
    Scheduler::new(
        Arc::new(SchedTask::new_kernel(0, KernelClass::Idle)),
        now_ns,
    )
}

#[def_test]
fn linux_integer_half_life_and_long_idle_vectors() {
    // Linux decay's n=0 table entry is 0xffffffff, not exactly 2^32.
    assert_eq!(decay(1024, 32), 511);
    assert_eq!(decay(u64::MAX, 32 * 63 + 1), 0);
    let mut pelt = Pelt::new(0);
    pelt.update(32 * PERIOD_NS, 1, 1, true, 1024);
    assert_eq!(
        pelt.avg,
        PeltAvg {
            load_avg: 512,
            runnable_avg: 512,
            util_avg: 512
        }
    );
    pelt.update(10_000 * PERIOD_NS, 0, 0, false, 1024);
    assert_eq!(pelt.avg, PeltAvg::default());
}

#[def_test]
fn linux_pelt_preserves_subunit_time_and_rejects_stale_samples() {
    let mut pelt = Pelt::new(0);
    pelt.update(1023, 1, 1, true, 1024);
    assert_eq!(pelt.last_update_ns, 0);
    pelt.update(1535, 1, 1, true, 1024);
    assert_eq!(pelt.last_update_ns, 1024);
    assert_eq!(pelt.period_contrib_ticks, 1);
    pelt.update(2048, 1, 1, true, 1024);
    assert_eq!(pelt.load_sum, 2);
    pelt.update(0, 0, 0, false, 1024);
    assert_eq!(pelt.last_update_ns, 0);
    assert_eq!(pelt.load_sum, 2);
}

#[def_test]
fn new_fair_tasks_share_the_remaining_utilization_estimate() {
    let mut rq = scheduler(0);
    for (id, expected) in [(1, 512), (2, 768), (3, 896)] {
        rq.enqueue(Arc::new(SchedTask::new(id)), EnqueueKind::New, 0);
        assert_eq!(rq.fair_pelt().util_avg, expected);
    }
    assert_eq!(rq.fair_pelt().load_avg, 3 * 1024);
}

#[def_test]
fn fair_waiting_history_is_distinct_from_execution_history() {
    let mut rq = scheduler(0);
    let runner = Arc::new(SchedTask::new(1));
    let waiter = Arc::new(SchedTask::new(2));
    rq.install_current(runner.clone(), 0);
    rq.enqueue(waiter.clone(), EnqueueKind::New, 0);
    rq.update(256 * PERIOD_NS, UpdateKind::Account);
    let running = rq.task_pelt(&runner);
    let waiting = rq.task_pelt(&waiter);
    assert!(running.util_avg > 1000);
    assert!(waiting.util_avg < 8);
    assert!(waiting.runnable_avg > 1000);
    assert!(rq.fair_pelt().runnable_avg > 2000);
}

#[def_test]
fn sleeping_history_decays_and_same_queue_wake_does_not_add_it_twice() {
    let mut rq = scheduler(0);
    let task = Arc::new(SchedTask::new(1));
    rq.install_current(task.clone(), 0);
    let now = 256 * PERIOD_NS;
    rq.update(now, UpdateKind::Account);
    rq.block_current(now);
    assert_eq!(Arc::strong_count(&task), 1);
    rq.update(now + 32 * PERIOD_NS, UpdateKind::Account);
    let before = rq.fair_pelt();
    assert!(before.util_avg > 490 && before.util_avg < 520);
    rq.enqueue(
        task.clone(),
        EnqueueKind::Wake { sync: false },
        now + 32 * PERIOD_NS,
    );
    assert_eq!(rq.fair_pelt(), before);
    assert!(rq.task_pelt(&task).util_avg > 490);
}

#[def_test]
fn sleeping_migration_transfers_history_across_different_decay_phases() {
    let mut source = scheduler(0);
    let mut destination = scheduler(PERIOD_NS / 2);
    let task = Arc::new(SchedTask::new(1));
    source.install_current(task.clone(), 0);
    let now = 256 * PERIOD_NS;
    source.update(now, UpdateKind::Account);
    source.block_current(now);
    source.detach_blocked_pelt(&task, now + 16 * PERIOD_NS);
    let history = source.task_pelt(&task);
    assert!(source.fair_pelt().util_avg <= 1);
    destination.enqueue(
        task.clone(),
        EnqueueKind::Wake { sync: false },
        now + 16 * PERIOD_NS,
    );
    assert_eq!(destination.fair_pelt().util_avg, history.util_avg);
    assert_eq!(destination.task_pelt(&task).util_avg, history.util_avg);
}

#[def_test]
fn nice_changes_only_weighted_history_and_pi_detaches_fair_history() {
    let mut rq = scheduler(0);
    let task = Arc::new(SchedTask::new(1));
    rq.install_current(task.clone(), 0);
    let now = 256 * PERIOD_NS;
    rq.update(now, UpdateKind::Account);
    let before = rq.task_pelt(&task);
    rq.set_attr(
        &task,
        SchedAttr {
            nice: Nice::new(5).unwrap(),
            ..SchedAttr::DEFAULT
        },
        now,
    )
    .unwrap();
    let after = rq.task_pelt(&task);
    assert!(after.load_avg < before.load_avg / 2);
    assert_eq!(after.util_avg, before.util_avg);
    assert_eq!(after.runnable_avg, before.runnable_avg);
    rq.set_pi_boost(&task, RtPrio::new(1).unwrap(), now);
    rq.update(now + 32 * PERIOD_NS, UpdateKind::Account);
    assert!(rq.fair_pelt().util_avg <= 1);
    assert!(rq.task_pelt(&task).util_avg < before.util_avg);
    rq.set_pi_boost(&task, RtPrio::NONE, now + 32 * PERIOD_NS);
    rq.exit_current(now + 32 * PERIOD_NS);
    assert!(rq.fair_pelt().util_avg <= 1);
}

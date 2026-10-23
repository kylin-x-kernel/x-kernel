// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Deterministic RCU observations when a wake probe retains the current task.

use ksched::{SchedAttr, SchedPolicy};
use kspin::NoPreemptIrqSave;
use unittest::{assert, assert_eq, def_test};

struct RetainedRunner {
    task: crate::KtaskRef,
    attr: SchedAttr,
    was_pending: bool,
}

impl RetainedRunner {
    fn new() -> Self {
        let task = crate::current().clone();
        let rq = crate::run_queue::current_run_queue::<NoPreemptIrqSave>();
        let attr = rq.inner.with_sched_rank(|scheduler| {
            let old = scheduler.attr(&task);
            scheduler
                .set_attr(
                    &task,
                    SchedAttr {
                        policy: SchedPolicy::Fifo,
                        rt_priority: ksched::RtPrio::MAX,
                        ..SchedAttr::DEFAULT
                    },
                    khal::time::monotonic_time_nanos(),
                )
                .expect("test current task admits FIFO");
            old
        });
        Self {
            was_pending: task.is_preempt_pending(),
            task,
            attr,
        }
    }

    fn probe(&self) -> bool {
        let mut rq = crate::run_queue::current_run_queue::<NoPreemptIrqSave>();
        // FIFO retains the runner without relying on elapsed fair-class time.
        // Verify the premise against the real scheduler before the real path.
        let decision = rq.inner.with_sched_rank(|scheduler| {
            scheduler.update(khal::time::monotonic_time_nanos(), ksched::UpdateKind::WakeProbe)
        });
        core::assert!(!decision.resched);
        self.task.set_preempt_pending(true);
        rq.preempt_resched();
        self.task.rcu_reader.is_enrolled()
    }
}

impl Drop for RetainedRunner {
    fn drop(&mut self) {
        let mut rq = crate::run_queue::current_run_queue::<NoPreemptIrqSave>();
        rq.inner.with_sched_rank(|scheduler| {
            scheduler
                .set_attr(&self.task, self.attr, khal::time::monotonic_time_nanos())
                .expect("restore test scheduling attributes");
        });
        self.task.set_preempt_pending(self.was_pending);
        rq.refresh_sched_deadline(khal::time::monotonic_time_nanos());
        crate::api::rearm_local_timer();
    }
}

#[def_test(serial)]
fn retained_runner_enrolls_nested_reader_until_outer_exit() {
    // Model the completed IRQ-tail checkpoint without an outer preempt credit.
    // No implicit protected borrow spans this fixture's IRQ-masked interval.
    let _irq = kspin::IrqSave::new();
    let runner = RetainedRunner::new();
    let task = &runner.task;
    let (outer, inner, after_inner, request_consumed) = task.rcu_reader.with_read(|_| {
        runner.probe();
        let outer = task.rcu_reader.is_enrolled();
        let inner = task.rcu_reader.with_read(|_| {
            runner.probe();
            task.rcu_reader.is_enrolled()
        });
        (outer, inner, task.rcu_reader.is_enrolled(), !task.is_preempt_pending())
    });
    assert!(outer && inner && after_inner);
    assert!(request_consumed);
    assert!(!task.rcu_reader.is_enrolled());
}

#[def_test(serial)]
fn retained_runner_cleans_deferred_reader_exit() {
    let _irq = kspin::IrqSave::new();
    let runner = RetainedRunner::new();
    let task = &runner.task;
    // SAFETY: the retained current reader is exclusively driven in watched task
    // context under IRQ exclusion. Each probe excludes preemption; the second
    // probe drains membership before restoring IRQs or releasing task storage.
    let was_active = unsafe {
        task.rcu_reader.with_read_deferred(|_| {
            runner.probe();
            task.rcu_reader.is_reading() && task.rcu_reader.is_enrolled()
        })
    };
    let was_pending = !task.rcu_reader.is_reading() && task.rcu_reader.is_enrolled();
    let remains_enrolled = runner.probe();
    assert!(was_active && was_pending);
    assert!(!remains_enrolled);
    assert!(!task.rcu_reader.is_enrolled());
}

#[def_test(serial)]
fn outer_preemption_guard_keeps_reader_and_request_unobserved() {
    let _irq = kspin::IrqSave::new();
    let runner = RetainedRunner::new();
    let task = &runner.task;
    let (was_enrolled, remains_pending, enrolled_after_release) = task.rcu_reader.with_read(|_| {
        let guard = kspin::NoPreempt::new();
        runner.probe();
        let was_enrolled = task.rcu_reader.is_enrolled();
        let remains_pending = task.is_preempt_pending();
        guard.release_without_check();
        runner.probe();
        (was_enrolled, remains_pending, task.rcu_reader.is_enrolled())
    });
    assert_eq!((was_enrolled, remains_pending, enrolled_after_release), (false, true, true));
    assert!(!task.rcu_reader.is_enrolled());
}

#[def_test(serial)]
fn irq_masked_task_checkpoint_keeps_the_observation_pending() {
    let _irq = kspin::IrqSave::new();
    let runner = RetainedRunner::new();
    let task = &runner.task;
    let observed = task.rcu_reader.with_read(|_| {
        task.set_preempt_pending(true);
        crate::check_preempt_pending();
        let before_tail = (task.rcu_reader.is_enrolled(), task.is_preempt_pending());
        // End the modeled implicit IRQ-off work before invoking its explicit
        // completed-tail checkpoint. The explicit reader still spans the tail.
        crate::TaskInner::current_check_preempt_pending_at_irq_exit();
        (before_tail, task.rcu_reader.is_enrolled(), task.is_preempt_pending())
    });
    assert_eq!(observed, ((false, true), true, false));
    assert!(!task.rcu_reader.is_enrolled());
}

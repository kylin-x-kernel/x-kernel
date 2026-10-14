// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Reader enrollment and storage lifetime across scheduler transitions.

use alloc::{boxed::Box, sync::Arc};
use core::{
    any::Any,
    sync::atomic::{AtomicBool, Ordering},
};

use kcpu_id_map::{KCpuMaskExt, LogicalCpuId};
use unittest::{assert, assert_eq, def_test};

struct RuntimeDropProbe(Arc<AtomicBool>);

impl crate::UserTaskRuntime for RuntimeDropProbe {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl Drop for RuntimeDropProbe {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

struct FirstEntryProbe {
    outgoing: crate::WeakKtaskRef,
    saw_abandoned_reader: Arc<AtomicBool>,
    has_entered: AtomicBool,
    is_reclaimed: Arc<AtomicBool>,
}

impl crate::UserTaskRuntime for FirstEntryProbe {
    fn on_enter(&self) {
        if self.has_entered.swap(true, Ordering::Relaxed) {
            return;
        }
        // The existing runtime callback precedes finish_task_switch. Inspect
        // the actual handoff, not a task that might have exited much earlier.
        let previous = super::PREV_TASK.shared_current().lock();
        let matches = previous.as_ref().is_some_and(|task| {
            self.outgoing.ptr_eq(&Arc::downgrade(task))
                && task.state() == crate::TaskState::Exited
                && task.rcu_reader.is_reading()
                && task.rcu_reader.is_enrolled()
        });
        self.saw_abandoned_reader.store(matches, Ordering::Release);
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl Drop for FirstEntryProbe {
    fn drop(&mut self) {
        self.is_reclaimed.store(true, Ordering::Release);
    }
}

#[def_test]
fn same_task_selection_still_records_the_reader() {
    let task = crate::current();
    let was_enrolled = task.rcu_reader.with_read(|_| {
        let rq = crate::run_queue::current_run_queue::<kspin::NoPreemptIrqSave>();
        // Model picking the already-running task using the actual same-task
        // switch path. No task state/owner is transferred to another task.
        rq.inner
            .switch_to(crate::current(), task.clone(), super::TaskState::Running);
        task.rcu_reader
            .is_enrolled_on(crate::rcu::reader_node(khal::percpu::this_cpu_id()))
    });
    assert!(was_enrolled);
    assert!(!task.rcu_reader.is_enrolled());
}

#[def_test]
fn abandoned_reader_is_unlinked_and_reclaimed_after_switch() {
    let passed = Arc::new(AtomicBool::new(false));
    let result = passed.clone();
    let cpu = LogicalCpuId::new(0);
    let supervisor = crate::prepare_task(crate::TaskInner::new_pidless_kthread(
        move || {
            let mut all_reclaimed = true;
            for _ in 0..4 {
                let runtime_dropped = Arc::new(AtomicBool::new(false));
                let abandoned = crate::prepare_task(crate::TaskInner::new_user(
                    || {
                        let task = crate::current();
                        task.rcu_reader.with_read::<()>(|_| {
                            task.rcu_reader.with_read::<()>(|_| crate::exit(17));
                        });
                    },
                    "rcu-abandoned".into(),
                    kidentity::allocate_root_pid_handle().expect("test task identity"),
                    Box::new(RuntimeDropProbe(runtime_dropped.clone())),
                ));
                abandoned.set_cpumask(crate::KCpuMask::one_shot_logical(cpu));
                crate::activate_task(&abandoned);
                let exit_code = abandoned.join();

                // This task enters the same CPU's FIFO exit queue after abandoned.
                // Its reclamation proves GC has visited the earlier entry without
                // needing a timing threshold or a production-path test hook.
                let sentinel = crate::prepare_task(crate::TaskInner::new_pidless_kthread(
                    || {},
                    "rcu-gc-sentinel".into(),
                    kbuild_config::TASK_STACK_SIZE,
                ));
                sentinel.set_cpumask(crate::KCpuMask::one_shot_logical(cpu));
                let reclaimed = Arc::downgrade(&sentinel);
                crate::activate_task(&sentinel);
                let sentinel_status = sentinel.join();
                drop(sentinel);
                while reclaimed.strong_count() != 0 {
                    crate::yield_now();
                }

                let is_clean =
                    !abandoned.rcu_reader.is_reading() && !abandoned.rcu_reader.is_enrolled();
                let retained = Arc::downgrade(&abandoned);
                drop(abandoned);
                // join only publishes exit; Weak expiration proves that neither
                // the switch owner nor GC permanently retains the exited task.
                while retained.strong_count() != 0 {
                    crate::yield_now();
                }
                // GC may be preempted after try_unwrap clears the strong
                // count but before it destroys the task's runtime field.
                while !runtime_dropped.load(Ordering::Acquire) {
                    crate::yield_now();
                }
                all_reclaimed &= exit_code == 17 && sentinel_status == 0 && is_clean;
            }
            result.store(all_reclaimed, Ordering::Release);
        },
        "rcu-retirement".into(),
        kbuild_config::TASK_STACK_SIZE,
    ));
    supervisor.set_cpumask(crate::KCpuMask::one_shot_logical(cpu));
    crate::activate_task(&supervisor);
    assert_eq!(supervisor.join(), 0);
    assert!(passed.load(Ordering::Acquire));
}

#[def_test]
fn first_task_entry_cleans_the_exited_readers_handoff() {
    let before = Arc::new(AtomicBool::new(false));
    let after = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    let before_probe = before.clone();
    let after_probe = after.clone();
    let finished_probe = finished.clone();
    let cpu = LogicalCpuId::new(0);
    let abandoned = crate::prepare_task(crate::TaskInner::new_pidless_kthread(
        move || {
            let task = crate::current();
            let outgoing = Arc::downgrade(&task);
            let outgoing_probe = outgoing.clone();
            let receiver = crate::prepare_task(crate::TaskInner::new_user(
                move || {
                    let is_clean = outgoing.upgrade().is_some_and(|old| {
                        !old.rcu_reader.is_reading() && !old.rcu_reader.is_enrolled()
                    });
                    after_probe.store(is_clean, Ordering::Release);
                },
                "rcu-first-entry".into(),
                kidentity::allocate_root_pid_handle().expect("test receiver identity"),
                Box::new(FirstEntryProbe {
                    outgoing: outgoing_probe,
                    saw_abandoned_reader: before_probe,
                    has_entered: AtomicBool::new(false),
                    is_reclaimed: finished_probe,
                }),
            ));
            receiver.set_cpumask(crate::KCpuMask::one_shot_logical(cpu));
            // FIFO outranks the ordinary supervisor/GC tasks. Keep scheduling
            // excluded from publication through exit so the receiver's first
            // switch-in must consume this task's terminal handoff.
            let _guard = kspin::NoPreemptIrqSave::new();
            {
                let rq = crate::run_queue::current_run_queue::<kspin::NoPreemptIrqSave>();
                let changed = rq.inner.with_sched(|scheduler| {
                    scheduler.set_attr(
                        &receiver,
                        ksched::SchedAttr {
                            policy: ksched::SchedPolicy::Fifo,
                            rt_priority: ksched::RtPrio::MAX,
                            ..ksched::SchedAttr::DEFAULT
                        },
                        khal::time::monotonic_time_nanos(),
                    )
                });
                core::assert!(changed.is_ok());
            }
            crate::activate_task(&receiver);
            // Do not strand a strong owner on the non-returning task's stack.
            drop(receiver);
            task.rcu_reader.with_read::<()>(|_| {
                task.rcu_reader.with_read::<()>(|_| crate::exit(19));
            });
        },
        "rcu-first-entry-outgoing".into(),
        kbuild_config::TASK_STACK_SIZE,
    ));
    abandoned.set_cpumask(crate::KCpuMask::one_shot_logical(cpu));
    crate::activate_task(&abandoned);
    let status = abandoned.join();
    // Wait for actual receiver-runtime destruction, not just its entry return.
    while !finished.load(Ordering::Acquire) {
        crate::yield_now();
    }
    assert_eq!(status, 19);
    assert!(before.load(Ordering::Acquire));
    assert!(after.load(Ordering::Acquire));
}

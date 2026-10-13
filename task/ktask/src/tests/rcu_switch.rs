// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Reader enrollment and storage lifetime across scheduler transitions.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use kcpu_id_map::{KCpuMaskExt, LogicalCpuId};
use unittest::{assert, assert_eq, def_test};

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
fn abandoned_reader_is_retained_without_stalling_gc() {
    let passed = Arc::new(AtomicBool::new(false));
    let result = passed.clone();
    let cpu = LogicalCpuId::new(0);
    let supervisor = crate::prepare_task(crate::TaskInner::new_pidless_kthread(
        move || {
            let abandoned = crate::prepare_task(crate::TaskInner::new_pidless_kthread(
                || {
                    let task = crate::current();
                    task.rcu_reader.with_read::<()>(|_| {
                        task.rcu_reader.with_read::<()>(|_| crate::exit(17));
                    });
                },
                "rcu-abandoned".into(),
                kbuild_config::TASK_STACK_SIZE,
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

            let is_queued = super::super::with_exited_tasks(|tasks| {
                tasks.iter().any(|task| Arc::ptr_eq(task, &abandoned))
            });
            let has_protection = abandoned.rcu_reader.is_reading()
                && abandoned
                    .rcu_reader
                    .is_enrolled_on(crate::rcu::reader_node(cpu));
            let retained = Arc::downgrade(&abandoned);
            drop(abandoned);
            // Deliberately permanent: the exited stack cannot run its guards.
            // This internal accounting test leaves one protected task/node for
            // the lifetime of the test kernel; it cannot share a GP-progress
            // acceptance run once a grace-period engine is integrated.
            result.store(
                exit_code == 17
                    && sentinel_status == 0
                    && !is_queued
                    && has_protection
                    && retained.strong_count() == 1,
                Ordering::Release,
            );
        },
        "rcu-retirement".into(),
        kbuild_config::TASK_STACK_SIZE,
    ));
    supervisor.set_cpumask(crate::KCpuMask::one_shot_logical(cpu));
    crate::activate_task(&supervisor);
    assert_eq!(supervisor.join(), 0);
    assert!(passed.load(Ordering::Acquire));
}

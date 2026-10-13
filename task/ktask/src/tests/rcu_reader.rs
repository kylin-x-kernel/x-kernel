// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Controlled task-context consumers of the internal reader accounting.

use alloc::sync::Arc;
#[cfg(feature = "smp")]
use core::sync::atomic::AtomicUsize;
use core::sync::atomic::{AtomicBool, Ordering};

use kcpu_id_map::{KCpuMaskExt, LogicalCpuId};
use unittest::{assert, assert_eq, def_test};

#[def_test]
fn rcu_scope_changes_sleepability_not_preemptibility() {
    let task = crate::current();
    assert!(crate::is_sleepable_context());
    let irq_enabled = karch::local_irq_enabled();
    let can_preempt = task.can_preempt(0);
    let (outer, inner, after_inner) = task.rcu_reader.with_read(|_| {
        let outer = (
            task.rcu_reader.is_reading(),
            crate::is_sleepable_context(),
            task.can_preempt(0),
            karch::local_irq_enabled(),
        );
        let inner = task
            .rcu_reader
            .with_read(|_| (task.rcu_reader.is_reading(), crate::is_sleepable_context()));
        (outer, inner, task.rcu_reader.is_reading())
    });
    assert_eq!(outer, (true, false, can_preempt, irq_enabled));
    assert_eq!(inner, (true, false));
    assert!(after_inner);
    let result = task
        .rcu_reader
        .with_read(|_| Err::<(), _>("normal error return"));
    assert_eq!(result, Err("normal error return"));
    assert!(!task.rcu_reader.is_reading());
    assert!(crate::is_sleepable_context());
}

#[def_test]
fn rcu_accounting_belongs_to_each_task() {
    let parent = crate::current().clone();
    let child = crate::spawn(move || {
        let task = crate::current();
        let initially_idle = !task.rcu_reader.is_reading();
        let isolated = task
            .rcu_reader
            .with_read(|_| task.rcu_reader.is_reading() && !parent.rcu_reader.is_reading());
        let finished_idle = !task.rcu_reader.is_reading();
        // exit does not unwind the callback stack or drop its captures.
        drop(parent);
        crate::exit(i32::from(!(initially_idle && isolated && finished_idle)));
    });
    assert_eq!(child.join(), 0);
}

#[def_test]
fn rcu_preempted_reader_remains_enrolled_until_outer_exit() {
    let worker = crate::spawn(|| {
        let cpu = LogicalCpuId::new(0);
        if !crate::set_current_affinity(crate::KCpuMask::one_shot_logical(cpu)) {
            crate::exit(1);
        }
        let task = crate::current().clone();
        let observed = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let observer_task = task.clone();
        let observer_result = observed.clone();
        let observer_finished = finished.clone();
        let observer = crate::prepare_task(crate::TaskInner::new_pidless_kthread(
            move || {
                observer_result.store(
                    observer_task.rcu_reader.is_reading()
                        && observer_task
                            .rcu_reader
                            .is_enrolled_on(crate::rcu::reader_node(cpu)),
                    Ordering::Release,
                );
                observer_finished.store(true, Ordering::Release);
            },
            "rcu-observer".into(),
            kbuild_config::TASK_STACK_SIZE,
        ));
        observer.set_cpumask(crate::KCpuMask::one_shot_logical(cpu));
        let still_enrolled = task.rcu_reader.with_read(|_| {
            task.rcu_reader.with_read(|_| {
                crate::activate_task(&observer);
                // Both tasks are pinned to CPU 0. The observer can run only
                // after a real preemption; this reader never blocks or yields.
                while !finished.load(Ordering::Acquire) {
                    task.set_preempt_pending(true);
                    crate::check_preempt_pending();
                    core::hint::spin_loop();
                }
            });
            task.rcu_reader.is_enrolled_on(crate::rcu::reader_node(cpu))
        });
        let passed =
            observed.load(Ordering::Acquire) && still_enrolled && !task.rcu_reader.is_enrolled();
        let observer_status = observer.join();
        // Release all owned test resources before the nonreturning exit.
        drop(observer);
        drop(finished);
        drop(observed);
        drop(task);
        crate::exit(i32::from(!passed || observer_status != 0));
    });
    let retired = Arc::downgrade(&worker);
    assert_eq!(worker.join(), 0);
    drop(worker);
    while retired.strong_count() != 0 {
        crate::yield_now();
    }
    assert_eq!(retired.strong_count(), 0);
}

#[cfg(feature = "smp")]
#[def_test]
fn rcu_migration_preserves_original_reader_node() {
    if kcpu_id_map::nr_cpus() < 2 {
        return unittest::TestResult::Ignored;
    }
    let started = Arc::new(AtomicBool::new(false));
    let finish = Arc::new(AtomicBool::new(false));
    let worker_started = started.clone();
    let worker_finish = finish.clone();
    let observed_cpu = Arc::new(AtomicUsize::new(usize::MAX));
    let worker_cpu = observed_cpu.clone();
    let worker = crate::prepare_task(crate::TaskInner::new_pidless_kthread(
        move || {
            let task = crate::current();
            task.rcu_reader.with_read(|_| {
                worker_started.store(true, Ordering::Release);
                while !worker_finish.load(Ordering::Acquire) {
                    worker_cpu.store(khal::percpu::this_cpu_id().as_usize(), Ordering::Release);
                    crate::check_preempt_pending();
                    core::hint::spin_loop();
                }
            });
            drop(worker_started);
            drop(worker_finish);
            drop(worker_cpu);
            crate::exit(i32::from(task.rcu_reader.is_enrolled()));
        },
        "rcu-migration".into(),
        kbuild_config::TASK_STACK_SIZE,
    ));
    let original = LogicalCpuId::new(0);
    worker.set_cpumask(crate::KCpuMask::one_shot_logical(original));
    crate::activate_task(&worker);
    while !started.load(Ordering::Acquire) {
        crate::yield_now();
    }
    let mut passed = true;
    for cpu in [1, 0, 1, 0] {
        let moved = crate::set_task_affinity(
            &worker,
            crate::KCpuMask::one_shot_logical(LogicalCpuId::new(cpu)),
        );
        if !moved {
            passed = false;
            break;
        }
        // Placement alone can leave the task merely queued on the new CPU.
        // Wait for the still-active reader itself to observe resumed execution.
        while observed_cpu.load(Ordering::Acquire) != cpu {
            crate::yield_now();
        }
        passed &= worker
            .rcu_reader
            .is_enrolled_on(crate::rcu::reader_node(original));
    }
    finish.store(true, Ordering::Release);
    let status = worker.join();
    assert!(passed);
    assert_eq!(status, 0);
    assert!(!worker.rcu_reader.is_enrolled());
}

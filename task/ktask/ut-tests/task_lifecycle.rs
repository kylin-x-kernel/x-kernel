// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Scheduler lifecycle regressions run against the boot-initialized kernel.
//! Joining every worker bounds fixture lifetime; the guest harness supplies
//! the hang bound, with no wall-clock latency assertion in these cases.

use alloc::{boxed::Box, format, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use unittest::{assert, assert_eq, def_test};

use crate::{WaitQueue, api as ktask};

struct DeferredRuntimeProbe(Arc<AtomicBool>);

impl crate::UserTaskRuntime for DeferredRuntimeProbe {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

impl Drop for DeferredRuntimeProbe {
    fn drop(&mut self) {
        core::assert!(crate::is_sleepable_context());
        crate::yield_now();
        self.0.store(true, Ordering::Release);
    }
}

#[def_test]
fn task_last_owner_under_irq_guard_defers_sleepable_runtime_drop() {
    let destroyed = Arc::new(AtomicBool::new(false));
    let task = crate::prepare_task(crate::TaskInner::new_user(
        || {},
        "deferred-runtime-drop".into(),
        kidentity::allocate_root_pid_handle().expect("test task identity"),
        Box::new(DeferredRuntimeProbe(destroyed.clone())),
    ));
    let weak = crate::KtaskRef::downgrade(&task);
    let retained = task.clone();
    drop(task);
    assert!(weak.upgrade().is_some());
    assert!(!destroyed.load(Ordering::Acquire));
    {
        let _guard = kspin::IrqSave::new();
        drop(retained);
        assert!(weak.upgrade().is_none());
        drop(weak);
    }
    while !destroyed.load(Ordering::Acquire) {
        crate::yield_now();
    }
}

#[def_test]
fn task_exit_without_external_owner_reclaims_after_stack_switch() {
    let destroyed = Arc::new(AtomicBool::new(false));
    let can_exit = Arc::new(AtomicBool::new(false));
    let worker_exit = can_exit.clone();
    let task = crate::prepare_task(crate::TaskInner::new_user(
        move || {
            while !worker_exit.load(Ordering::Acquire) {
                crate::yield_now();
            }
        },
        "exit-without-owner".into(),
        kidentity::allocate_root_pid_handle().expect("test task identity"),
        Box::new(DeferredRuntimeProbe(destroyed.clone())),
    ));
    let weak = crate::KtaskRef::downgrade(&task);
    crate::activate_task(&task);
    drop(task);
    assert!(!destroyed.load(Ordering::Acquire));
    can_exit.store(true, Ordering::Release);
    while !destroyed.load(Ordering::Acquire) {
        crate::yield_now();
    }
    assert!(weak.upgrade().is_none());
}

#[def_test]
fn exited_task_last_external_owner_can_release_under_irq_guard() {
    let destroyed = Arc::new(AtomicBool::new(false));
    let task = crate::prepare_task(crate::TaskInner::new_user(
        || {},
        "retained-exited-task".into(),
        kidentity::allocate_root_pid_handle().expect("test task identity"),
        Box::new(DeferredRuntimeProbe(destroyed.clone())),
    ));
    let weak = crate::KtaskRef::downgrade(&task);
    crate::activate_task(&task);
    assert_eq!(task.join(), 0);
    while crate::KtaskRef::strong_count(&task) != 1 {
        crate::yield_now();
    }
    let other_destroyed = Arc::new(AtomicBool::new(false));
    let other = crate::prepare_task(crate::TaskInner::new_user(
        || {},
        "independent-exited-task".into(),
        kidentity::allocate_root_pid_handle().expect("test task identity"),
        Box::new(DeferredRuntimeProbe(other_destroyed.clone())),
    ));
    crate::activate_task(&other);
    assert_eq!(other.join(), 0);
    drop(other);
    while !other_destroyed.load(Ordering::Acquire) {
        crate::yield_now();
    }
    assert!(!destroyed.load(Ordering::Acquire));
    {
        let _guard = kspin::IrqSave::new();
        drop(task);
        assert!(weak.upgrade().is_none());
    }
    while !destroyed.load(Ordering::Acquire) {
        crate::yield_now();
    }
}

#[def_test]
fn test_spawn_yield_completes() {
    let finished = Arc::new(AtomicUsize::new(0));
    let tasks: Vec<_> = (0..10)
        .map(|i| {
            let finished = finished.clone();
            ktask::spawn_with_name(
                move || {
                    ktask::yield_now();
                    finished.fetch_add(1, Ordering::Release);
                },
                format!("spawn-yield-{i}"),
            )
        })
        .collect();
    for task in tasks {
        assert_eq!(task.join(), 0);
    }
    assert_eq!(finished.load(Ordering::Acquire), 10);
}

#[def_test]
fn test_fp_state_switch() {
    const FLOATS: [f64; 5] = [
        core::f64::consts::PI,
        core::f64::consts::E,
        -core::f64::consts::SQRT_2,
        0.0,
        0.618033988749895,
    ];
    let tasks: Vec<_> = FLOATS.into_iter().enumerate().map(|(i, float)| {
        ktask::spawn(move || {
            let mut value = core::hint::black_box(float) + i as f64;
            ktask::yield_now();
            value = core::hint::black_box(value) - i as f64;
            core::assert!((value - float).abs() < 1e-9);
        })
    }).collect();
    for task in tasks {
        assert_eq!(task.join(), 0);
    }
}

#[def_test]
fn test_wait_queue() {
    let ready = Arc::new(WaitQueue::new());
    let release = Arc::new(WaitQueue::new());
    let counter = Arc::new(AtomicUsize::new(0));
    let is_released = Arc::new(AtomicBool::new(false));
    let tasks: Vec<_> = (0..10).map(|_| {
        let (ready, release, counter, is_released) =
            (ready.clone(), release.clone(), counter.clone(), is_released.clone());
        ktask::spawn(move || {
            counter.fetch_add(1, Ordering::Release);
            ready.notify_one(true);
            // The persistent predicate also handles release before enrollment.
            release.wait_until(|| is_released.load(Ordering::Acquire));
            counter.fetch_sub(1, Ordering::Release);
        })
    }).collect();
    ready.wait_until(|| counter.load(Ordering::Acquire) == 10);
    assert_eq!(counter.load(Ordering::Acquire), 10);
    is_released.store(true, Ordering::Release);
    release.notify_all(true);
    for task in tasks {
        assert_eq!(task.join(), 0);
    }
    assert_eq!(counter.load(Ordering::Acquire), 0);
}

#[def_test]
fn test_task_join() {
    let tasks: Vec<_> = (0..10).map(|i| {
        ktask::spawn(move || {
            ktask::yield_now();
            ktask::exit(i);
        })
    }).collect();
    for (i, task) in tasks.into_iter().enumerate() {
        assert_eq!(task.join(), i as i32);
    }
}

#[def_test]
fn test_prepare_task_requires_activation() {
    let executed = Arc::new(AtomicBool::new(false));
    let worker_executed = executed.clone();
    let task = ktask::prepare_task(
        crate::TaskInner::new_kthread(
            move || worker_executed.store(true, Ordering::Release),
            "prepared-task".into(),
            0x1000,
        ).expect("fixture kernel thread identity allocation succeeds"),
    );
    ktask::yield_now();
    assert!(!executed.load(Ordering::Acquire));
    ktask::activate_task(&task);
    assert_eq!(task.join(), 0);
    assert!(executed.load(Ordering::Acquire));
}

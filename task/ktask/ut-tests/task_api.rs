// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Task execution provider regression and shared mutex integration tests.
//!
//! Contention uses task/completion handshakes, never guest latency thresholds.
//! Run under an external QEMU timeout to diagnose stalled scheduler progress.
//! Only migration requires SMP; blocking and exclusion also run on UP.

use alloc::{boxed::Box, sync::Arc};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kcompletion::Completion;
use super::Mutex;
use unittest::{assert, assert_eq, assert_ne, def_test};

#[def_test]
fn current_task_presence_query_preserves_irq_exclusion() {
    let _irq = kspin::IrqSave::new();
    let is_present = kiface_task_api::TaskExecutionIf::has_current_task();
    let is_irq_masked = !karch::local_irq_enabled();
    assert!(is_present);
    assert!(is_irq_masked);
}

#[def_test]
fn completion_provider_blocks_until_completion() {
    let completion = Arc::new(Completion::new());
    let finished = Arc::new(AtomicBool::new(false));
    let worker = crate::spawn({
        let completion = completion.clone();
        let finished = finished.clone();
        move || {
            kiface_task_wait_api::wait_for_completion(&completion).unwrap();
            finished.store(true, Ordering::Release);
        }
    });
    while !matches!(worker.state(), crate::TaskState::Blocked | crate::TaskState::Exited) {
        crate::yield_now();
    }
    let was_blocked = worker.state() == crate::TaskState::Blocked;
    let completed_before_wake = finished.load(Ordering::Acquire);
    completion.complete_all();
    assert_eq!(worker.join(), 0);
    assert!(was_blocked);
    assert!(!completed_before_wake);
    assert!(finished.load(Ordering::Acquire));
}

#[def_test]
fn completion_preparation_rejects_before_body_and_allows_reuse() {
    let completion = Completion::new();
    completion.complete_all();
    let mut calls = 0;
    let rejected = {
        let _irq = kspin::IrqSave::new();
        kiface_task_wait_api::TaskCompletionWaitIf::with_prepared_wait(&mut |_| calls += 1)
    };
    assert_eq!(rejected, Err(kerrno::KError::InvalidInput));
    assert_eq!(calls, 0);
    kiface_task_wait_api::TaskCompletionWaitIf::with_prepared_wait(&mut |wait| {
        calls += 1;
        wait.wait_for_completion(&completion);
        wait.wait_for_completion(&completion);
    }).unwrap();
    assert_eq!(calls, 1);
}

#[def_test]
fn preemption_provider_balances_nested_credits_without_changing_irq_state() {
    use kiface_task_api::TaskPreemptionIf;

    let task = crate::current();
    let irq_enabled = karch::local_irq_enabled();
    let was_unprotected = task.can_preempt(0);
    TaskPreemptionIf::disable_preempt();
    let first_credit = task.can_preempt(1);
    TaskPreemptionIf::disable_preempt();
    let nested_credit = task.can_preempt(2);
    TaskPreemptionIf::enable_preempt_without_check();
    let outer_credit_retained = task.can_preempt(1);
    let irq_unchanged = karch::local_irq_enabled() == irq_enabled;
    TaskPreemptionIf::enable_preempt();
    // Restore all credits before assertions so failure cannot strand exclusion.
    assert!(was_unprotected);
    assert!(first_credit);
    assert!(nested_credit);
    assert!(outer_credit_retained);
    assert!(irq_unchanged);
    assert!(task.can_preempt(0));
    assert_eq!(karch::local_irq_enabled(), irq_enabled);
}

/// Retains the default unknown identity token without constructing a process.
struct UnknownTokenRuntime;

impl crate::UserTaskRuntime for UnknownTokenRuntime {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

#[def_test]
fn unknown_token_task_uses_nonzero_owner_key() {
    let task = crate::TaskInner::new_user(
        || {},
        "task-api-unknown-token".into(),
        kidentity::allocate_root_pid_handle().expect("test task identity"),
        Box::new(UnknownTokenRuntime),
    );
    assert_eq!(task.owner_key(), 0);
    let key = super::owner_key_for_task(&task);
    assert_ne!(key.get(), 0);
    assert_eq!(key.get(), &task as *const crate::TaskInner as usize as u64);
    assert_eq!(super::owner_key_for_task(&task), key);
}

#[def_test]
fn guard_releases_mutex_and_preserves_protected_value() {
    let lock = Mutex::new(0);
    {
        let mut guard = lock.lock();
        *guard += 1;
    }
    let mut guard = lock.try_lock().unwrap();
    assert_eq!(*guard, 1);
    *guard += 1;
    drop(guard);
    assert_eq!(*lock.lock(), 2);
}

#[def_test]
fn held_mutex_rejects_try_lock_and_other_instances_progress() {
    let lock = Mutex::new(0);
    let other = Mutex::new(0);
    let guard = lock.lock();
    let is_rejected = lock.try_lock().is_none();
    *other.lock() += 1;
    drop(guard);
    assert!(is_rejected);
    assert_eq!(*other.lock(), 1);
    assert!(lock.try_lock().is_some());
}

#[def_test]
fn cross_task_try_lock_rejects_until_guard_drops() {
    let lock = Arc::new(Mutex::new(0));
    let rejected = Arc::new(AtomicBool::new(false));
    let guard = lock.lock();
    let task = {
        let lock = lock.clone();
        let rejected = rejected.clone();
        crate::spawn(move || {
            rejected.store(lock.try_lock().is_none(), Ordering::Release);
        })
    };
    // Only the nonwaiting contender is joined while exclusion is held.
    let status = task.join();
    drop(guard);
    let worker_lock = lock.clone();
    let worker = crate::spawn(move || *worker_lock.lock() += 1);
    let worker_status = worker.join();
    assert_eq!([status, worker_status], [0, 0]);
    assert!(rejected.load(Ordering::Acquire));
    assert_eq!(*lock.lock(), 1);
}

#[def_test]
fn blocked_waiter_acquires_only_after_guard_drops() {
    let lock = Arc::new(Mutex::new(0));
    let started = Arc::new(Completion::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let guard = lock.lock();
    let worker = {
        let lock = lock.clone();
        let started = started.clone();
        let calls = calls.clone();
        crate::spawn(move || {
            started.complete_all();
            let mut guard = lock.lock();
            *guard += 1;
            calls.fetch_add(1, Ordering::Release);
        })
    };
    crate::completion::wait_for_completion(&started).unwrap();
    // Completion proves the worker passed startup; the only subsequent wait
    // before its increment is this mutex, so Blocked proves actual contention.
    while !matches!(
        worker.state(),
        crate::TaskState::Blocked | crate::TaskState::Exited
    ) {
        crate::yield_now();
    }
    let was_blocked = worker.state() == crate::TaskState::Blocked;
    let calls_while_owned = calls.load(Ordering::Acquire);
    let value_while_owned = *guard;
    drop(guard);
    let status = worker.join();
    assert_eq!(status, 0);
    assert!(was_blocked);
    assert_eq!(calls_while_owned, 0);
    assert_eq!(value_while_owned, 0);
    assert_eq!(calls.load(Ordering::Acquire), 1);
    assert_eq!(*lock.lock(), 1);
}

#[cfg(feature = "smp")]
#[def_test]
fn guard_keeps_exclusion_across_task_migration() {
    use kcpu_id_map::{KCpuMaskExt, LogicalCpuId};

    if kcpu_id_map::nr_cpus() < 2 {
        return unittest::TestResult::Ignored;
    }
    let observations = Arc::new(Mutex::new(None));
    let worker_observations = observations.clone();
    let worker = crate::spawn(move || {
        let source = LogicalCpuId::new(0);
        let destination = LogicalCpuId::new(1);
        if !crate::set_current_affinity(crate::KCpuMask::one_shot_logical(source)) {
            return;
        }
        let lock = Mutex::new(0);
        let mut guard = lock.lock();
        let before = khal::percpu::this_cpu_id();
        let rejected_before = lock.try_lock().is_none();
        *guard = 1;
        // The guard remains on the same task's stack across migration; it is
        // never sent to another task. Record actual placement, not just affinity.
        let did_migrate =
            crate::set_current_affinity(crate::KCpuMask::one_shot_logical(destination));
        let after = khal::percpu::this_cpu_id();
        let rejected_after = lock.try_lock().is_none();
        let retained_value = *guard;
        drop(guard);
        let reacquired = lock.try_lock().map(|mut guard| {
            *guard += 1;
            *guard
        });
        *worker_observations.lock() = Some((
            did_migrate,
            before,
            after,
            rejected_before,
            rejected_after,
            retained_value,
            reacquired,
        ));
    });
    assert_eq!(worker.join(), 0);
    let (did_migrate, before, after, rejected_before, rejected_after, retained_value, reacquired) =
        observations.lock().take().unwrap();
    assert!(did_migrate);
    assert_eq!(
        (before, after),
        (LogicalCpuId::new(0), LogicalCpuId::new(1))
    );
    assert!(rejected_before);
    assert!(rejected_after);
    assert_eq!(retained_value, 1);
    assert_eq!(reacquired, Some(2));
}

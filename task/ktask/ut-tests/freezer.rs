// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Real scheduler coverage: acknowledgments, persistent freeze and rollback.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use unittest::{assert, assert_eq, def_test};

use super::*;
use crate::WaitQueue;

pub(super) static SERIAL: klazy::Lazy<kmutex::Mutex<()>> = klazy::Lazy::new(|| kmutex::Mutex::new(()));

pub(super) fn wait_for_request(progress: &Progress, task: &KtaskRef) {
    CompletionWait::prepare()
        .unwrap()
        .wait_until(&progress.changed, || task.freeze_request.lock().is_some());
}

#[def_test]
fn entry_acknowledges_and_ordinary_wakeup_cannot_thaw() {
    let _serial = SERIAL.lock();
    if kcpu_id_map::nr_cpus() != 1 {
        return unittest::TestResult::Ignored;
    }
    let ran = Arc::new(AtomicBool::new(false));
    let worker_ran = ran.clone();
    let task = crate::prepare_task(crate::TaskInner::new_pidless_kthread(
        move || worker_ran.store(true, Ordering::Release),
        "freeze-entry".into(),
        0x4000,
    ));
    let cancellation = FreezeCancellation::new();
    let start = cancellation.clone();
    let target = task.clone();
    let activator = crate::spawn(move || {
        wait_for_request(&start.0, &target);
        crate::activate_task(&target);
    });
    let guard = freeze_tasks(
        core::slice::from_ref(&task),
        TimeSpan::from_secs(5),
        &cancellation,
    )
    .unwrap();
    assert!(guard.requests[0].1.is_confirmed.load(Ordering::Acquire));
    assert!(!ran.load(Ordering::Acquire));
    cancellation.cancel();
    crate::wake_task(&task, false);
    crate::yield_now();
    assert!(!ran.load(Ordering::Acquire));
    assert!(matches!(
        freeze_tasks(&[], TimeSpan::ZERO, &FreezeCancellation::new()),
        Err(FreezeError::Busy)
    ));
    drop(guard);
    assert_eq!(task.try_join().unwrap(), 0);
    assert_eq!(activator.try_join().unwrap(), 0);
    assert!(ran.load(Ordering::Acquire));
    drop(freeze_tasks(&[], TimeSpan::ZERO, &FreezeCancellation::new()).unwrap());
}

#[def_test]
fn blocked_task_is_not_acknowledged_and_timeout_cleans_requests() {
    let _serial = SERIAL.lock();
    if kcpu_id_map::nr_cpus() != 1 {
        return unittest::TestResult::Ignored;
    }
    let ready = Arc::new(WaitQueue::new());
    let released = Arc::new(AtomicBool::new(false));
    let started = Arc::new(AtomicBool::new(false));
    let worker_started = started.clone();
    let worker_ready = ready.clone();
    let worker_release = released.clone();
    let task = crate::spawn(move || {
        worker_started.store(true, Ordering::Release);
        worker_ready.notify_all(false);
        worker_ready.wait_until(|| worker_release.load(Ordering::Acquire));
    });
    ready.wait_until(|| started.load(Ordering::Acquire));
    let result = freeze_tasks(
        core::slice::from_ref(&task),
        TimeSpan::ZERO,
        &FreezeCancellation::new(),
    );
    assert!(matches!(result, Err(FreezeError::TimedOut)));
    assert!(task.freeze_request.lock().is_none());
    released.store(true, Ordering::Release);
    ready.notify_all(false);
    assert_eq!(task.try_join().unwrap(), 0);
}

#[def_test]
fn cancellation_wakes_owner_and_releases_partial_freeze() {
    let _serial = SERIAL.lock();
    if kcpu_id_map::nr_cpus() != 1 {
        return unittest::TestResult::Ignored;
    }
    let release = Arc::new(AtomicBool::new(false));
    let queue = Arc::new(WaitQueue::new());
    let started = Arc::new(AtomicBool::new(false));
    let worker_started = started.clone();
    let worker_release = release.clone();
    let worker_queue = queue.clone();
    let blocked = crate::spawn(move || {
        worker_started.store(true, Ordering::Release);
        worker_queue.notify_all(false);
        worker_queue.wait_until(|| worker_release.load(Ordering::Acquire));
    });
    queue.wait_until(|| started.load(Ordering::Acquire));
    let ran = Arc::new(AtomicBool::new(false));
    let worker_ran = ran.clone();
    let cooperative = crate::prepare_task(crate::TaskInner::new_pidless_kthread(
        move || worker_ran.store(true, Ordering::Release),
        "freeze-cancel".into(),
        0x4000,
    ));
    let cancellation = FreezeCancellation::new();
    let cancel = cancellation.clone();
    let target = cooperative.clone();
    let canceller = crate::spawn(move || {
        wait_for_request(&cancel.0, &target);
        let request = target.freeze_request.lock().clone().unwrap();
        crate::activate_task(&target);
        CompletionWait::prepare()
            .unwrap()
            .wait_until(&request.round.progress.changed, || {
                request.is_confirmed.load(Ordering::Acquire)
            });
        cancel.cancel();
    });
    let result = freeze_tasks(
        &[cooperative.clone(), blocked.clone()],
        TimeSpan::from_secs(5),
        &cancellation,
    );
    assert!(matches!(result, Err(FreezeError::Cancelled)));
    assert!(cooperative.freeze_request.lock().is_none());
    assert!(blocked.freeze_request.lock().is_none());
    release.store(true, Ordering::Release);
    queue.notify_all(false);
    assert_eq!(blocked.try_join().unwrap(), 0);
    assert_eq!(cooperative.try_join().unwrap(), 0);
    assert_eq!(canceller.try_join().unwrap(), 0);
    assert!(ran.load(Ordering::Acquire));
}

#[def_test]
fn validation_rejects_self_duplicates_and_pre_cancelled_attempt() {
    let _serial = SERIAL.lock();
    if kcpu_id_map::nr_cpus() != 1 {
        return unittest::TestResult::Ignored;
    }
    let task = crate::prepare_task(crate::TaskInner::new_pidless_kthread(
        || {},
        "freeze-invalid".into(),
        0x4000,
    ));
    let cancellation = FreezeCancellation::new();
    assert!(matches!(
        freeze_tasks(&[crate::current().clone()], TimeSpan::ZERO, &cancellation),
        Err(FreezeError::InvalidTaskSet)
    ));
    assert!(matches!(
        freeze_tasks(&[task.clone(), task.clone()], TimeSpan::ZERO, &cancellation),
        Err(FreezeError::InvalidTaskSet)
    ));
    cancellation.cancel();
    assert!(matches!(
        freeze_tasks(core::slice::from_ref(&task), TimeSpan::ZERO, &cancellation),
        Err(FreezeError::Cancelled)
    ));
    assert!(task.freeze_request.lock().is_none());
    crate::activate_task(&task);
    assert_eq!(task.try_join().unwrap(), 0);
    drop(
        freeze_tasks(
            core::slice::from_ref(&task),
            TimeSpan::ZERO,
            &FreezeCancellation::new(),
        )
        .unwrap(),
    );
}

#[def_test]
fn freezer_checks_context_before_topology_and_does_not_park_remote_tasks() {
    let _serial = SERIAL.lock();
    let cancellation = FreezeCancellation::new();
    {
        let _irq = kspin::IrqSave::new();
        assert!(matches!(
            freeze_tasks(&[], TimeSpan::ZERO, &cancellation),
            Err(FreezeError::InvalidContext)
        ));
    }
    if kcpu_id_map::nr_cpus() != 1 {
        assert!(matches!(
            freeze_tasks(&[], TimeSpan::ZERO, &cancellation),
            Err(FreezeError::MultipleCpus)
        ));
    }
}

#[def_test]
fn exit_notifies_waiting_coordinator_without_false_blocked_acknowledgment() {
    let _serial = SERIAL.lock();
    if kcpu_id_map::nr_cpus() != 1 {
        return unittest::TestResult::Ignored;
    }
    let started = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let queue = Arc::new(WaitQueue::new());
    let worker_started = started.clone();
    let worker_release = release.clone();
    let worker_queue = queue.clone();
    let task = crate::spawn(move || {
        worker_started.store(true, Ordering::Release);
        worker_queue.notify_all(false);
        worker_queue.wait_until(|| worker_release.load(Ordering::Acquire));
        // No checkpoint here: only permanent exit can confirm this task.
    });
    queue.wait_until(|| started.load(Ordering::Acquire));
    let cancellation = FreezeCancellation::new();
    let progress = cancellation.clone();
    let target = task.clone();
    let releaser = crate::spawn(move || {
        wait_for_request(&progress.0, &target);
        release.store(true, Ordering::Release);
        queue.notify_all(false);
    });
    let frozen = freeze_tasks(
        core::slice::from_ref(&task),
        TimeSpan::from_secs(5),
        &cancellation,
    )
    .unwrap();
    assert_eq!(task.state(), TaskState::Exited);
    assert!(!frozen.requests[0].1.is_confirmed.load(Ordering::Acquire));
    drop(frozen);
    assert_eq!(task.try_join().unwrap(), 0);
    assert_eq!(releaser.try_join().unwrap(), 0);
}

#[def_test]
fn running_kernel_worker_acknowledges_at_explicit_safe_point() {
    let _serial = SERIAL.lock();
    if kcpu_id_map::nr_cpus() != 1 {
        return unittest::TestResult::Ignored;
    }
    let started = Arc::new(AtomicBool::new(false));
    let proceed = Arc::new(AtomicBool::new(false));
    let ran = Arc::new(AtomicBool::new(false));
    let queue = Arc::new(WaitQueue::new());
    let worker_started = started.clone();
    let worker_proceed = proceed.clone();
    let worker_ran = ran.clone();
    let worker_queue = queue.clone();
    let task = crate::spawn(move || {
        worker_started.store(true, Ordering::Release);
        worker_queue.notify_all(false);
        worker_queue.wait_until(|| worker_proceed.load(Ordering::Acquire));
        checkpoint().unwrap();
        worker_ran.store(true, Ordering::Release);
    });
    queue.wait_until(|| started.load(Ordering::Acquire));
    let cancellation = FreezeCancellation::new();
    let progress = cancellation.clone();
    let target = task.clone();
    let releaser = crate::spawn(move || {
        wait_for_request(&progress.0, &target);
        proceed.store(true, Ordering::Release);
        queue.notify_all(false);
    });
    let frozen = freeze_tasks(
        core::slice::from_ref(&task),
        TimeSpan::from_secs(5),
        &cancellation,
    )
    .unwrap();
    assert!(frozen.requests[0].1.is_confirmed.load(Ordering::Acquire));
    assert!(!ran.load(Ordering::Acquire));
    drop(frozen);
    assert_eq!(task.try_join().unwrap(), 0);
    assert_eq!(releaser.try_join().unwrap(), 0);
    assert!(ran.load(Ordering::Acquire));
}

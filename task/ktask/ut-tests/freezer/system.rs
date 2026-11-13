// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Serial scheduler admission tests never suspend hardware or change global time.
//! Serial execution keeps unrelated parallel-test task creation out of snapshots.

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};

use unittest::{assert, assert_eq, def_test};

use super::*;
use crate::{TaskInner, TaskState, WaitQueue};

fn prepared(name: &str, entry: impl FnOnce() + Send + 'static) -> KtaskRef {
    crate::prepare_task(TaskInner::new_pidless_kthread(entry, name.into(), 0x4000))
}

fn contains(tasks: &[KtaskRef], target: &KtaskRef) -> bool {
    tasks.iter().any(|task| KtaskRef::ptr_eq(task, target))
}

// Keep boot/reclamation/worker infrastructure running in scheduler-only tests.
// This exemption fixture is not used by production PM and never powers devices off.
fn freeze_except(
    target: Option<&KtaskRef>,
    timeout: TimeSpan,
    cancellation: &FreezeCancellation,
) -> Result<SystemFrozenTasks, FreezeError> {
    let gate = PublicationGate::acquire().unwrap();
    let current = crate::current().clone();
    let exemptions: Vec<_> = gate
        .snapshot()
        .into_iter()
        .filter(|task| {
            !KtaskRef::ptr_eq(task, &current)
                && !target.is_some_and(|target| KtaskRef::ptr_eq(task, target))
        })
        .collect();
    // Keep admission closed across both snapshots: even asynchronous boot
    // workers cannot become unlisted non-cooperative targets in between.
    freeze_with_gate(gate, &exemptions, timeout, cancellation)
}

#[def_test(serial)]
fn system_admission_defers_activation_and_wakeup_until_guard_release() {
    let _serial = super::super::tests::SERIAL.lock();
    let cancellation = FreezeCancellation::new();
    let guard = freeze_except(None, TimeSpan::from_secs(5), &cancellation).unwrap();
    let ran = Arc::new(AtomicBool::new(false));
    let worker_ran = ran.clone();
    let task = prepared("pm-deferred", move || {
        worker_ran.store(true, Ordering::Release)
    });
    crate::activate_task(&task);
    crate::activate_task(&task);
    crate::wake_task(&task, true);
    crate::yield_now();
    assert!(!ran.load(Ordering::Acquire));
    assert_eq!(task.state(), TaskState::Ready);
    assert!(matches!(PublicationGate::acquire(), Err(FreezeError::Busy)));
    cancellation.cancel();
    assert!(!ran.load(Ordering::Acquire));
    drop(guard);
    assert_eq!(task.try_join().unwrap(), 0);
    assert!(ran.load(Ordering::Acquire));
}

#[def_test(serial)]
fn exemption_snapshot_keeps_new_activation_deferred_through_freeze() {
    let _serial = super::super::tests::SERIAL.lock();
    let gate = PublicationGate::acquire().unwrap();
    let current = crate::current().clone();
    let exemptions: Vec<_> = gate
        .snapshot()
        .into_iter()
        .filter(|task| !KtaskRef::ptr_eq(task, &current))
        .collect();
    let ran = Arc::new(AtomicBool::new(false));
    let task_ran = ran.clone();
    // Deliberately activate a non-cooperative task between exemption selection
    // and freezing. It must remain deferred under the original gate.
    let task = crate::spawn(move || task_ran.store(true, Ordering::Release));
    let guard = freeze_with_gate(
        gate,
        &exemptions,
        TimeSpan::from_secs(5),
        &FreezeCancellation::new(),
    )
    .unwrap();
    assert!(!ran.load(Ordering::Acquire));
    assert!(matches!(PublicationGate::acquire(), Err(FreezeError::Busy)));
    drop(guard);
    assert_eq!(task.try_join().unwrap(), 0);
    assert!(ran.load(Ordering::Acquire));
}

#[def_test(serial)]
fn snapshot_tracks_admitted_tasks_but_not_detached_or_retired_tasks() {
    let _serial = super::super::tests::SERIAL.lock();
    let detached = prepared("pm-detached", || {});
    let gate = PublicationGate::acquire().unwrap();
    assert!(!contains(&gate.snapshot(), &detached));
    assert!(contains(&gate.snapshot(), &crate::current().clone()));
    drop(gate);
    crate::activate_task(&detached);
    assert_eq!(detached.try_join().unwrap(), 0);
    while !detached.is_exit_complete.load(Ordering::Acquire) {
        crate::yield_now();
    }
    let gate = PublicationGate::acquire().unwrap();
    assert!(!contains(&gate.snapshot(), &detached));
}

#[def_test(serial)]
fn snapshot_includes_activation_admitted_before_scheduler_enqueue() {
    let _serial = super::super::tests::SERIAL.lock();
    let task = prepared("pm-publish-gap", || {});
    assert!(admit(&task));
    let gate = PublicationGate::acquire().unwrap();
    assert!(contains(&gate.snapshot(), &task));
    drop(gate);
    // Complete the activation interval deliberately paused above. Production
    // activate_task performs these exact two operations in the same order.
    crate::select_run_queue::<kspin::NoPreemptIrqSave>(&task).add_task(task.clone());
    assert_eq!(task.try_join().unwrap(), 0);
}

#[def_test(serial)]
fn user_task_cannot_be_exempted_even_when_registered() {
    let _serial = super::super::tests::SERIAL.lock();
    struct Runtime;
    impl crate::UserTaskRuntime for Runtime {
        fn as_any(&self) -> &dyn core::any::Any {
            self
        }
    }
    let task = crate::prepare_task(TaskInner::new_user(
        crate::PreparedTaskEntry::new_nowait(|| {}).unwrap(),
        "pm-user-exemption".into(),
        kidentity::allocate_root_pid_handle().unwrap(),
        kalloc_types::KBox::new(Runtime).unwrap(),
    ));
    assert!(admit(&task));
    let result = freeze_system_tasks(
        core::slice::from_ref(&task),
        TimeSpan::ZERO,
        &FreezeCancellation::new(),
    );
    // The fixture never enters a scheduler queue; retire its test-owned
    // admission before asserting, including when exemption validation fails.
    retire(&task);
    assert!(matches!(result, Err(FreezeError::InvalidTaskSet)));
    drop(PublicationGate::acquire().unwrap());
}

#[def_test(serial)]
fn task_admitted_before_closure_cannot_escape_system_freezing() {
    let _serial = super::super::tests::SERIAL.lock();
    let entered = Arc::new(AtomicBool::new(false));
    let proceed = Arc::new(AtomicBool::new(false));
    let ran = Arc::new(AtomicBool::new(false));
    let queue = Arc::new(WaitQueue::new());
    let task = {
        let entered = entered.clone();
        let proceed = proceed.clone();
        let ran = ran.clone();
        let queue = queue.clone();
        crate::spawn(move || {
            entered.store(true, Ordering::Release);
            queue.notify_all(false);
            queue.wait_until(|| proceed.load(Ordering::Acquire));
            super::super::checkpoint().unwrap();
            ran.store(true, Ordering::Release);
        })
    };
    queue.wait_until(|| entered.load(Ordering::Acquire));
    let cancellation = FreezeCancellation::new();
    let target = task.clone();
    let progress = cancellation.clone();
    let newborn_ran = Arc::new(AtomicBool::new(false));
    let newborn = Arc::new(kspin::SpinNoIrq::new(None));
    let child_slot = newborn.clone();
    let child_ran = newborn_ran.clone();
    let helper = crate::spawn(move || {
        super::super::tests::wait_for_request(&progress.0, &target);
        // An explicitly exempt producer may still create a task while the
        // freezer waits. Its first activation must not escape this round.
        let child = crate::spawn(move || child_ran.store(true, Ordering::Release));
        *child_slot.lock() = Some(child);
        proceed.store(true, Ordering::Release);
        queue.notify_all(false);
    });
    let guard = freeze_except(Some(&task), TimeSpan::from_secs(5), &cancellation).unwrap();
    assert!(!ran.load(Ordering::Acquire));
    assert!(!newborn_ran.load(Ordering::Acquire));
    assert!(
        task.freeze_request
            .lock()
            .as_ref()
            .is_some_and(|request| { request.is_confirmed.load(Ordering::Acquire) })
    );
    drop(guard);
    let child = newborn.lock().take().unwrap();
    assert_eq!(child.try_join().unwrap(), 0);
    assert_eq!(task.try_join().unwrap(), 0);
    assert_eq!(helper.try_join().unwrap(), 0);
    assert!(ran.load(Ordering::Acquire));
}

#[def_test(serial)]
fn system_freeze_timeout_reopens_publication_and_cleans_requests() {
    let _serial = super::super::tests::SERIAL.lock();
    let entered = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let queue = Arc::new(WaitQueue::new());
    let task = {
        let entered = entered.clone();
        let release = release.clone();
        let queue = queue.clone();
        crate::spawn(move || {
            entered.store(true, Ordering::Release);
            queue.notify_all(false);
            queue.wait_until(|| release.load(Ordering::Acquire));
        })
    };
    queue.wait_until(|| entered.load(Ordering::Acquire));
    assert!(matches!(
        freeze_except(Some(&task), TimeSpan::ZERO, &FreezeCancellation::new()),
        Err(FreezeError::TimedOut)
    ));
    assert!(task.freeze_request.lock().is_none());
    drop(PublicationGate::acquire().unwrap());
    let after = crate::spawn(|| {});
    assert_eq!(after.try_join().unwrap(), 0);
    release.store(true, Ordering::Release);
    queue.notify_all(false);
    assert_eq!(task.try_join().unwrap(), 0);
}

#[def_test(serial)]
fn rejected_exemptions_and_cancellation_release_system_admission() {
    let _serial = super::super::tests::SERIAL.lock();
    let current = crate::current().clone();
    let detached = prepared("pm-invalid-exempt", || {});
    for exempt in [current, detached] {
        assert!(matches!(
            freeze_system_tasks(&[exempt], TimeSpan::ZERO, &FreezeCancellation::new()),
            Err(FreezeError::InvalidTaskSet)
        ));
        drop(PublicationGate::acquire().unwrap());
    }
    let cancellation = FreezeCancellation::new();
    cancellation.cancel();
    assert!(matches!(
        freeze_except(None, TimeSpan::ZERO, &cancellation),
        Err(FreezeError::Cancelled)
    ));
    drop(PublicationGate::acquire().unwrap());
}

#[def_test(serial)]
fn gate_rollback_keeps_deferred_task_alive_without_external_owner() {
    let _serial = super::super::tests::SERIAL.lock();
    let ran = Arc::new(AtomicBool::new(false));
    let queue = Arc::new(WaitQueue::new());
    let gate = PublicationGate::acquire().unwrap();
    let worker_ran = ran.clone();
    let worker_queue = queue.clone();
    let task = crate::spawn(move || {
        worker_ran.store(true, Ordering::Release);
        worker_queue.notify_all(false);
    });
    let weak = KtaskRef::downgrade(&task);
    drop(task);
    assert!(!ran.load(Ordering::Acquire));
    drop(gate);
    queue.wait_until(|| ran.load(Ordering::Acquire));
    // A wake can preempt the worker before permanent exit. Do not leave that
    // task in the next serial test's infrastructure snapshot.
    if let Some(task) = weak.upgrade() {
        assert_eq!(task.try_join().unwrap(), 0);
    }
}

#[def_test(serial)]
fn system_freezer_rejects_invalid_context() {
    let _serial = super::super::tests::SERIAL.lock();
    let cancellation = FreezeCancellation::new();
    {
        let _irq = kspin::IrqSave::new();
        assert!(matches!(
            freeze_system_tasks(&[], TimeSpan::ZERO, &cancellation),
            Err(FreezeError::InvalidContext)
        ));
    }
}

#[def_test(serial)]
fn registry_links_once_and_retires_only_its_members() {
    let _serial = super::super::tests::SERIAL.lock();
    let task = prepared("pm-registry-contract", || {});
    let mut registry = Registry::new();
    let active = Arc::new(Registration::new(&task));
    let never_admitted = Arc::new(Registration::new(&task));

    // A non-member is never passed to the list's member-only removal.
    assert!(registry.retire(&never_admitted).is_none());
    assert!(!never_admitted.links.is_linked());

    // Repeated admission must not relink, which the list rejects by panicking.
    assert!(registry.admit(&active, &task));
    assert!(!registry.admit(&active, &task));
    assert_eq!(registry.active.iter().count(), 1);

    assert!(registry.retire(&active).is_some());
    assert!(!active.links.is_linked());
    assert!(registry.active.is_empty());
    assert!(registry.retire(&active).is_none());
    assert!(!registry.admit(&active, &task));

    registry.is_closed = true;
    let deferred = Arc::new(Registration::new(&task));
    assert!(!registry.admit(&deferred, &task));
    // A deferred registration is on the other list; retire must leave it there.
    assert!(registry.retire(&deferred).is_none());
    assert!(deferred.links.is_linked());
    assert!(registry.deferred.pop_front().is_some());
    assert!(registry.deferred.is_empty());

    crate::activate_task(&task);
    assert_eq!(task.try_join().unwrap(), 0);
}

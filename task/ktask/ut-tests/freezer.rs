// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Real scheduler coverage: acknowledgments, persistent freeze and rollback.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use unittest::{assert, assert_eq, def_test};

use super::*;
use crate::WaitQueue;

pub(super) static SERIAL: klazy::Lazy<kmutex::Mutex<()>> =
    klazy::Lazy::new(|| kmutex::Mutex::new(()));

pub(super) fn wait_for_request(progress: &Progress, task: &KtaskRef) {
    CompletionWait::prepare()
        .unwrap()
        .wait_until(&progress.changed, || task.freeze_request.lock().is_some());
}

#[def_test(serial)]
fn entry_acknowledges_and_ordinary_wakeup_cannot_thaw() {
    let _serial = SERIAL.lock();
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

#[def_test(serial)]
fn blocked_task_is_not_acknowledged_and_timeout_cleans_requests() {
    let _serial = SERIAL.lock();
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

#[def_test(serial)]
fn cancellation_wakes_owner_and_releases_partial_freeze() {
    let _serial = SERIAL.lock();
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

#[def_test(serial)]
fn validation_rejects_self_duplicates_and_pre_cancelled_attempt() {
    let _serial = SERIAL.lock();
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

#[def_test(serial)]
fn freezer_rejects_invalid_context_and_accepts_empty_smp_set() {
    let _serial = SERIAL.lock();
    let cancellation = FreezeCancellation::new();
    {
        let _irq = kspin::IrqSave::new();
        assert!(matches!(
            freeze_tasks(&[], TimeSpan::ZERO, &cancellation),
            Err(FreezeError::InvalidContext)
        ));
    }
    drop(freeze_tasks(&[], TimeSpan::ZERO, &cancellation).unwrap());
}

#[def_test(serial)]
fn exit_notifies_waiting_coordinator_without_false_blocked_acknowledgment() {
    let _serial = SERIAL.lock();
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

#[def_test(serial)]
fn running_kernel_worker_acknowledges_at_explicit_safe_point() {
    let _serial = SERIAL.lock();
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

struct FreezeSwitchProbe(AtomicBool);

// SAFETY: callbacks only inspect the current task's IRQ-safe request slot and
// atomics. They do not allocate, wait, schedule or acquire scheduler locks.
unsafe impl kiface_task_api::TaskScheduleObserver for FreezeSwitchProbe {
    unsafe fn schedule_in(&self, _: kcpu_id_map::LogicalCpuId) {}

    unsafe fn schedule_out(
        &self,
        _: kcpu_id_map::LogicalCpuId,
        _: kiface_task_api::ScheduleOutReason,
    ) {
        let task = crate::current();
        if task.state() == TaskState::Frozen {
            let request = task.freeze_request.lock().clone().unwrap();
            core::assert!(!request.is_confirmed.load(Ordering::Acquire));
            self.0.store(true, Ordering::Release);
        }
    }
}

#[def_test(serial)]
fn confirmation_follows_outgoing_observer_and_completed_stack_switch() {
    let _serial = SERIAL.lock();
    let probe = Arc::new(FreezeSwitchProbe(AtomicBool::new(false)));
    let task = crate::prepare_task(
        crate::TaskInner::new_pidless_kthread(|| {}, "freeze-switch".into(), 0x4000)
            .with_schedule_observer(probe.clone()),
    );
    let cancellation = FreezeCancellation::new();
    let progress = cancellation.clone();
    let target = task.clone();
    let activator = crate::spawn(move || {
        wait_for_request(&progress.0, &target);
        crate::activate_task(&target);
    });
    let frozen = freeze_tasks(
        core::slice::from_ref(&task),
        TimeSpan::from_secs(30),
        &cancellation,
    )
    .unwrap();
    assert!(probe.0.load(Ordering::Acquire));
    assert_eq!(task.state(), TaskState::Frozen);
    #[cfg(feature = "smp")]
    assert!(!task.on_cpu());
    drop(frozen);
    assert_eq!(task.try_join().unwrap(), 0);
    assert_eq!(activator.try_join().unwrap(), 0);
}

#[def_test(serial)]
fn late_switch_completion_cannot_confirm_a_replacement_request() {
    let _serial = SERIAL.lock();
    let _admission = Admission::acquire().unwrap();
    let make_request = || {
        Arc::new(Request {
            round: Arc::new(Round {
                progress: FreezeCancellation::new().0,
                is_released: AtomicBool::new(false),
            }),
            is_confirmed: AtomicBool::new(false),
            has_entered: AtomicBool::new(true),
        })
    };
    // An unpublished task models the old handoff without racing a real CPU.
    let task = crate::prepare_task(crate::TaskInner::new_pidless_kthread(
        || {},
        "freeze-round-identity".into(),
        0x4000,
    ));
    let old = make_request();
    *task.freeze_request.lock() = Some(old.clone());
    task.set_state(TaskState::Frozen);
    let completion = SwitchCompletion::prepare(&task);
    old.round.is_released.store(true, Ordering::Release);
    let next = make_request();
    *task.freeze_request.lock() = Some(next.clone());
    completion.complete(&task);
    assert!(old.is_confirmed.load(Ordering::Acquire));
    assert!(!next.is_confirmed.load(Ordering::Acquire));
    task.freeze_request.lock().take();
    task.set_state(TaskState::Ready);
}

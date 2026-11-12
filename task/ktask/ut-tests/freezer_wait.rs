// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Scoped real-task waits; no clocks, runtime providers or IRQ hooks are replaced.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use unittest::{assert, assert_eq, def_test};

use super::*;
use crate::{WaitQueue, future::PreparedTaskWait};

fn until_blocked(task: &KtaskRef) {
    while task.state() != TaskState::Blocked {
        crate::yield_now();
    }
}

#[def_test(serial)]
fn idle_freezable_wait_acknowledges_without_work_wake_and_retains_work_hint() {
    let returned = Arc::new(AtomicBool::new(false));
    let wake_slot = Arc::new(kspin::SpinNoIrq::new(None));
    let slot = wake_slot.clone();
    let done = returned.clone();
    let task = crate::spawn(move || {
        let mut wait = PreparedTaskWait::prepare().unwrap();
        *slot.lock() = Some(wait.waker());
        wait.park_freezable().unwrap();
        core::assert!(!crate::current().is_freezer_wait.load(Ordering::Acquire));
        done.store(true, Ordering::Release);
    });
    until_blocked(&task);
    let cancellation = FreezeCancellation::new();
    let frozen = freeze_tasks(
        core::slice::from_ref(&task),
        TimeSpan::from_secs(5),
        &cancellation,
    )
    .unwrap();
    assert!(frozen.requests[0].1.is_confirmed.load(Ordering::Acquire));
    wake_slot.lock().as_ref().unwrap().wake_by_ref();
    crate::wake_task(&task, false);
    crate::yield_now();
    assert!(!returned.load(Ordering::Acquire));
    cancellation.cancel();
    assert!(!returned.load(Ordering::Acquire));
    drop(frozen);
    assert_eq!(task.try_join().unwrap(), 0);
    assert!(returned.load(Ordering::Acquire));
}

#[def_test(serial)]
fn request_before_freezable_park_is_confirmed_and_work_hint_survives_thaw() {
    let ready = Arc::new(WaitQueue::new());
    let proceed = Arc::new(AtomicBool::new(false));
    let wake_slot = Arc::new(kspin::SpinNoIrq::new(None));
    let slot = wake_slot.clone();
    let worker_ready = ready.clone();
    let worker_proceed = proceed.clone();
    let task = crate::spawn(move || {
        let mut wait = PreparedTaskWait::prepare().unwrap();
        *slot.lock() = Some(wait.waker());
        worker_ready.wait_until(|| worker_proceed.load(Ordering::Acquire));
        wait.park_freezable().unwrap();
    });
    until_blocked(&task);
    let cancellation = FreezeCancellation::new();
    let progress = cancellation.clone();
    let target = task.clone();
    let helper = crate::spawn(move || {
        super::tests::wait_for_request(&progress.0, &target);
        proceed.store(true, Ordering::Release);
        ready.notify_all(false);
    });
    let frozen = freeze_tasks(
        core::slice::from_ref(&task),
        TimeSpan::from_secs(5),
        &cancellation,
    )
    .unwrap();
    assert!(frozen.requests[0].1.is_confirmed.load(Ordering::Acquire));
    wake_slot.lock().as_ref().unwrap().wake_by_ref();
    drop(frozen);
    assert_eq!(task.try_join().unwrap(), 0);
    assert_eq!(helper.try_join().unwrap(), 0);
}

#[def_test(serial)]
fn cancellation_releases_idle_acknowledgment_and_clears_wait_opt_in() {
    let task = crate::spawn(|| {
        PreparedTaskWait::prepare()
            .unwrap()
            .park_freezable()
            .unwrap();
        core::assert!(!crate::current().is_freezer_wait.load(Ordering::Acquire));
    });
    until_blocked(&task);
    let release = Arc::new(AtomicBool::new(false));
    let queue = Arc::new(WaitQueue::new());
    let worker_queue = queue.clone();
    let worker_release = release.clone();
    let blocked =
        crate::spawn(move || worker_queue.wait_until(|| worker_release.load(Ordering::Acquire)));
    until_blocked(&blocked);
    let cancellation = FreezeCancellation::new();
    let cancel = cancellation.clone();
    let target = task.clone();
    let helper = crate::spawn(move || {
        super::tests::wait_for_request(&cancel.0, &target);
        let request = target.freeze_request.lock().clone().unwrap();
        CompletionWait::prepare()
            .unwrap()
            .wait_until(&request.round.progress.changed, || {
                request.is_confirmed.load(Ordering::Acquire)
            });
        cancel.cancel();
    });
    assert!(matches!(
        freeze_tasks(
            &[task.clone(), blocked.clone()],
            TimeSpan::from_secs(5),
            &cancellation
        ),
        Err(FreezeError::Cancelled)
    ));
    assert_eq!(task.try_join().unwrap(), 0);
    release.store(true, Ordering::Release);
    queue.notify_all(false);
    assert_eq!(blocked.try_join().unwrap(), 0);
    assert_eq!(helper.try_join().unwrap(), 0);
}

#[def_test(serial)]
fn freezable_wait_rejects_invalid_context_before_opt_in() {
    let mut wait = PreparedTaskWait::prepare().unwrap();
    let guard = kspin::NoPreemptIrqSave::new();
    assert_eq!(wait.park_freezable(), Err(FreezeError::InvalidContext));
    assert!(!crate::current().is_freezer_wait.load(Ordering::Acquire));
    drop(guard);
    let guard = kirq::context::local_bh_disable();
    assert_eq!(wait.park_freezable(), Err(FreezeError::InvalidContext));
    assert!(!crate::current().is_freezer_wait.load(Ordering::Acquire));
    drop(guard);
}

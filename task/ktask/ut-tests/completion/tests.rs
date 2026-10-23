// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use unittest::{assert, assert_eq, def_test};

use super::*;

#[def_test(serial)]
fn completion_wait_consumes_early_tokens_and_broadcast() {
    let completion = Completion::new();
    let wait = CompletionWait::prepare().unwrap();
    completion.complete();
    wait.wait(&completion);
    assert!(!completion.try_wait());
    completion.complete_all();
    wait.wait(&completion);
    wait.wait(&completion);
    assert!(completion.is_completed());
    completion.reinit();
    assert!(!completion.try_wait());
}

#[def_test(serial)]
fn completion_wait_rejects_non_sleepable_context_even_if_complete() {
    let completion = Completion::new();
    completion.complete_all();
    {
        let _guard = NoPreemptIrqSave::new();
        assert!(matches!(
            CompletionWait::prepare(),
            Err(KError::InvalidInput)
        ));
        assert_eq!(wait_for_completion(&completion), Err(KError::InvalidInput));
        assert!(!kirq::IrqSyncWaitIf::can_wait());
        assert!(matches!(
            kirq::IrqSyncWaitIf::wait_for_completion(&completion),
            Err(kirq::IrqDescError::InvalidContext { .. })
        ));
    }
    {
        let _guard = kirq::context::local_bh_disable();
        assert!(matches!(
            CompletionWait::prepare(),
            Err(KError::InvalidInput)
        ));
    }
    assert!(wait_for_completion(&completion).is_ok());
}

#[def_test(serial)]
fn completion_wait_parks_and_broadcast_wakes_all_joiners() {
    let completion = Arc::new(Completion::new());
    let finished = Arc::new(AtomicUsize::new(0));
    let tasks: alloc::vec::Vec<_> = (0..3)
        .map(|_| {
            let completion = completion.clone();
            let finished = finished.clone();
            crate::spawn(move || {
                let wait = CompletionWait::prepare().unwrap();
                wait.wait(&completion);
                finished.fetch_add(1, Ordering::Release);
            })
        })
        .collect();
    while tasks
        .iter()
        .any(|task| task.state() != crate::TaskState::Blocked)
    {
        crate::yield_now();
    }
    assert_eq!(finished.load(Ordering::Acquire), 0);
    {
        let _guard = NoPreemptIrqSave::new();
        assert_eq!(completion.complete_all(), 3);
    }
    for task in tasks {
        assert_eq!(task.join(), 0);
    }
    assert_eq!(finished.load(Ordering::Acquire), 3);
}

#[def_test(serial)]
fn predicate_wait_rechecks_spurious_wakes_and_reuses_handle() {
    let queue = Arc::new(WaitQueue::new());
    let ready = Arc::new(AtomicBool::new(false));
    let checks = Arc::new(AtomicUsize::new(0));
    let task = crate::spawn({
        let queue = queue.clone();
        let ready = ready.clone();
        let checks = checks.clone();
        move || {
            let wait = CompletionWait::prepare().unwrap();
            wait.wait_until(&queue, || {
                checks.fetch_add(1, Ordering::Release);
                ready.load(Ordering::Acquire)
            });
            wait.wait_until(&queue, || ready.load(Ordering::Acquire));
        }
    });
    while task.state() != crate::TaskState::Blocked {
        crate::yield_now();
    }
    let before = checks.load(Ordering::Acquire);
    select_wake_run_queue::<NoPreemptIrqSave>(&task).unblock_task(task.clone(), true);
    while checks.load(Ordering::Acquire) == before || task.state() != crate::TaskState::Blocked {
        crate::yield_now();
    }
    assert_eq!(queue.notify_all(), 1);
    ready.store(true, Ordering::Release);
    queue.notify_all();
    assert_eq!(task.join(), 0);
    assert!(checks.load(Ordering::Acquire) > before);
}

#[def_test(serial)]
fn predicate_wait_registers_before_check_and_checks_outside_queue_lock() {
    let queue = WaitQueue::new();
    let wait = CompletionWait::prepare().unwrap();
    let mut checks = 0;
    let mut notified = 0;
    wait.wait_until(&queue, || {
        checks += 1;
        if checks == 1 {
            notified = queue.notify_all();
            false
        } else {
            true
        }
    });
    assert_eq!(checks, 2);
    assert_eq!(notified, 1);
    assert_eq!(queue.notify_all(), 0);
}

#[def_test(serial)]
fn join_waits_for_exit_and_preserves_exit_code() {
    let release = Arc::new(Completion::new());
    let exit_code = Arc::new(core::sync::atomic::AtomicI32::new(0));
    let exiting = crate::spawn({
        let release = release.clone();
        move || {
            wait_for_completion(&release).unwrap();
            crate::exit(37);
        }
    });
    {
        let _guard = NoPreemptIrqSave::new();
        assert_eq!(exiting.try_join(), Err(KError::InvalidInput));
    }
    assert_ne!(exiting.state(), crate::TaskState::Exited);
    let joiner = crate::spawn({
        let exiting = exiting.clone();
        let exit_code = exit_code.clone();
        move || exit_code.store(exiting.join(), Ordering::Release)
    });
    while joiner.state() != crate::TaskState::Blocked {
        crate::yield_now();
    }
    release.complete_all();
    assert_eq!(joiner.join(), 0);
    assert_eq!(exit_code.load(Ordering::Acquire), 37);
    assert_eq!(exiting.join(), 37);
    {
        let _guard = NoPreemptIrqSave::new();
        assert_eq!(exiting.try_join(), Ok(37));
    }
}

struct CompleteAndJoin {
    completion: Arc<Completion>,
    tasks: alloc::vec::Vec<crate::KtaskRef>,
}

impl Drop for CompleteAndJoin {
    fn drop(&mut self) {
        self.completion.complete_all();
        for task in &self.tasks {
            task.join();
        }
    }
}

#[def_test(serial)]
fn counted_completion_wakes_one_task_per_token_and_publishes_data() {
    let completion = Arc::new(Completion::new());
    let acknowledged = Arc::new(Completion::new());
    let payload = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new([const { AtomicUsize::new(0) }; 4]);
    let finished = Arc::new(AtomicUsize::new(0));
    let mut cleanup = CompleteAndJoin {
        completion: completion.clone(),
        tasks: alloc::vec::Vec::new(),
    };
    for index in 0..4 {
        cleanup.tasks.push(crate::spawn({
            let completion = completion.clone();
            let acknowledged = acknowledged.clone();
            let payload = payload.clone();
            let observed = observed.clone();
            let finished = finished.clone();
            move || {
                let wait = CompletionWait::prepare().unwrap();
                wait.wait(&completion);
                observed[index].store(payload.load(Ordering::Relaxed), Ordering::Relaxed);
                finished.fetch_add(1, Ordering::Relaxed);
                acknowledged.complete();
            }
        }));
    }
    while cleanup
        .tasks
        .iter()
        .any(|task| task.state() != crate::TaskState::Blocked)
    {
        crate::yield_now();
    }
    payload.store(0xc0ffee, Ordering::Relaxed);
    let wait = CompletionWait::prepare().unwrap();
    for expected in 1..=4 {
        assert_eq!(completion.complete(), 1);
        wait.wait(&acknowledged);
        assert_eq!(finished.load(Ordering::Relaxed), expected);
        assert!(!completion.try_wait());
    }
    for value in observed.iter() {
        assert_eq!(value.load(Ordering::Relaxed), 0xc0ffee);
    }
}

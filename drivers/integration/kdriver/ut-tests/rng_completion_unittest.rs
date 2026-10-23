// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kspin::NoPreemptIrqSave;
use unittest::{assert, assert_eq, def_test};

use super::*;

fn completion() -> Arc<HostRngCompletion> {
    Arc::new(HostRngCompletion {
        waiters: WaitQueue::new(),
    })
}

// The test runner supplies the watchdog; correctness uses task handshakes,
// not an assumed number of milliseconds or scheduling turns.
fn wait_for(mut condition: impl FnMut() -> bool) {
    while !condition() {
        ktask::yield_now();
    }
}

struct FinishReaderOnDrop {
    task: ktask::KtaskRef,
    provider: Arc<HostRngCompletion>,
    is_ready: Arc<AtomicBool>,
}

impl Drop for FinishReaderOnDrop {
    fn drop(&mut self) {
        self.is_ready.store(true, Ordering::Release);
        self.provider.notify();
        self.task.join();
    }
}

#[def_test(serial)]
fn early_notification_and_notification_before_park_are_not_lost() {
    let provider = new_rng_completion();
    provider.notify();
    assert_eq!(provider.wait_until(&mut || true), Ok(()));

    let provider = completion();
    let mut wake_count = None;
    let result = provider.wait_until(&mut || {
        wake_count = Some(provider.waiters.notify_all());
        true
    });
    assert_eq!(result, Ok(()));
    assert_eq!(wake_count, Some(1));

    let provider = completion();
    let mut checks = 0;
    let result = provider.wait_until(&mut || {
        checks += 1;
        if checks == 1 {
            // Model an IRQ arriving after registration but before park.
            provider.notify();
            false
        } else {
            true
        }
    });
    assert_eq!(result, Ok(()));
    assert_eq!(checks, 2);
    assert_eq!(provider.waiters.notify_all(), 0);
}

#[def_test(serial)]
fn blocked_reader_rechecks_spurious_wake_and_notification() {
    let provider = completion();
    let is_ready = Arc::new(AtomicBool::new(false));
    let checks = Arc::new(AtomicUsize::new(0));
    let succeeded = Arc::new(AtomicBool::new(false));
    let task = ktask::spawn({
        let provider = provider.clone();
        let is_ready = is_ready.clone();
        let checks = checks.clone();
        let succeeded = succeeded.clone();
        move || {
            let result = provider.wait_until(&mut || {
                checks.fetch_add(1, Ordering::Release);
                is_ready.load(Ordering::Acquire)
            });
            succeeded.store(result.is_ok(), Ordering::Release);
        }
    });
    let _cleanup = FinishReaderOnDrop {
        task: task.clone(),
        provider: provider.clone(),
        is_ready: is_ready.clone(),
    };
    wait_for(|| task.state() == ktask::TaskState::Blocked);
    let before = checks.load(Ordering::Acquire);
    // The coordinator runs while the reader is genuinely blocked.
    assert!(before > 0);
    // Request rescheduling so the synthetic spurious wake is observed even if
    // wake placement chooses another CPU.
    ktask::wake_task(&task, true);
    wait_for(|| {
        checks.load(Ordering::Acquire) > before && task.state() == ktask::TaskState::Blocked
    });
    let before = checks.load(Ordering::Acquire);
    provider.notify(); // A notification alone must not manufacture success.
    wait_for(|| {
        checks.load(Ordering::Acquire) > before && task.state() == ktask::TaskState::Blocked
    });
    assert!(!succeeded.load(Ordering::Acquire));
    is_ready.store(true, Ordering::Release);
    {
        let _guard = NoPreemptIrqSave::new();
        provider.notify();
    }
    wait_for(|| task.state() == ktask::TaskState::Exited);
    assert_eq!(task.join(), 0);
    assert!(succeeded.load(Ordering::Acquire));
    assert_eq!(provider.waiters.notify_all(), 0);
}

#[def_test(serial)]
fn task_interruption_does_not_cancel_rng_wait() {
    let provider = completion();
    let is_ready = Arc::new(AtomicBool::new(false));
    let checks = Arc::new(AtomicUsize::new(0));
    let succeeded = Arc::new(AtomicBool::new(false));
    let task = ktask::spawn({
        let provider = provider.clone();
        let is_ready = is_ready.clone();
        let checks = checks.clone();
        let succeeded = succeeded.clone();
        move || {
            let result = provider.wait_until(&mut || {
                checks.fetch_add(1, Ordering::Release);
                is_ready.load(Ordering::Acquire)
            });
            succeeded.store(result.is_ok(), Ordering::Release);
        }
    });
    let _cleanup = FinishReaderOnDrop {
        task: task.clone(),
        provider: provider.clone(),
        is_ready: is_ready.clone(),
    };
    wait_for(|| task.state() == ktask::TaskState::Blocked);
    let before = checks.load(Ordering::Acquire);
    ktask::interrupt_task(&task, true);
    wait_for(|| {
        checks.load(Ordering::Acquire) > before && task.state() == ktask::TaskState::Blocked
    });
    assert!(!succeeded.load(Ordering::Acquire));
    is_ready.store(true, Ordering::Release);
    provider.notify();
    wait_for(|| task.state() == ktask::TaskState::Exited);
    assert_eq!(task.join(), 0);
    assert!(succeeded.load(Ordering::Acquire));
    assert_eq!(provider.waiters.notify_all(), 0);
    provider.notify(); // Late device notification after task exit is harmless.
    assert_eq!(provider.wait_until(&mut || true), Ok(()));
}

#[def_test(serial)]
fn atomic_context_is_rejected_before_the_driver_predicate() {
    let provider = completion();
    let mut checks = 0;
    let result = {
        let _guard = NoPreemptIrqSave::new();
        provider.wait_until(&mut || {
            checks += 1;
            true
        })
    };
    assert_eq!(result, Err(DriverError::InvalidInput));
    assert_eq!(checks, 0);
    assert_eq!(provider.waiters.notify_all(), 0);
}

#[def_test(serial)]
fn one_notification_rechecks_all_blocked_readers() {
    let provider = completion();
    let is_ready = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicUsize::new(0));
    let spawn_reader = || {
        let provider = provider.clone();
        let is_ready = is_ready.clone();
        let finished = finished.clone();
        ktask::spawn(move || {
            provider
                .wait_until(&mut || is_ready.load(Ordering::Acquire))
                .unwrap();
            finished.fetch_add(1, Ordering::Release);
        })
    };
    let first = spawn_reader();
    let second = spawn_reader();
    let _first_cleanup = FinishReaderOnDrop {
        task: first.clone(),
        provider: provider.clone(),
        is_ready: is_ready.clone(),
    };
    let _second_cleanup = FinishReaderOnDrop {
        task: second.clone(),
        provider: provider.clone(),
        is_ready: is_ready.clone(),
    };
    wait_for(|| {
        first.state() == ktask::TaskState::Blocked && second.state() == ktask::TaskState::Blocked
    });
    is_ready.store(true, Ordering::Release);
    provider.notify();
    assert_eq!(first.join(), 0);
    assert_eq!(second.join(), 0);
    assert_eq!(finished.load(Ordering::Acquire), 2);
    assert_eq!(provider.waiters.notify_all(), 0);
}

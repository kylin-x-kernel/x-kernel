// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use unittest::{assert, assert_eq, def_test};

use super::*;

#[def_test(serial)]
fn prepared_wait_consumes_early_wake_and_reuses_waker() {
    let mut wait = PreparedTaskWait::prepare().unwrap();
    let first = wait.waker();
    let second = wait.waker();
    assert!(first.will_wake(&second));
    for _ in 0..3 {
        first.wake_by_ref();
        wait.park();
        assert!(!*wait.kwaker.woke.lock());
    }
    drop(wait);
    // Escaped notifications must not borrow the retired parking handle.
    second.wake_by_ref();
}

#[def_test(serial)]
fn prepared_wait_rejects_irq_mask_and_bh_disable() {
    {
        let _guard = NoPreemptIrqSave::new();
        assert!(matches!(
            PreparedTaskWait::prepare(),
            Err(KError::InvalidInput)
        ));
    }
    {
        let _guard = kirq::context::local_bh_disable();
        assert!(matches!(
            PreparedTaskWait::prepare(),
            Err(KError::InvalidInput)
        ));
    }
    assert!(PreparedTaskWait::prepare().is_ok());
}

#[cfg(feature = "preempt")]
#[def_test(serial)]
fn prepared_wait_rejects_preempt_disable() {
    let _guard = kspin::NoPreempt::new();
    assert!(matches!(
        PreparedTaskWait::prepare(),
        Err(KError::InvalidInput)
    ));
}

#[def_test(serial)]
fn prepared_wait_rejects_softirq() {
    use kirq::softirq::{
        SoftirqVec,
        test_support::{ScopedDaemonWakeGate, ScopedSoftirqAction},
    };

    static REJECTED: AtomicBool = AtomicBool::new(false);
    fn action() {
        REJECTED.store(
            matches!(PreparedTaskWait::prepare(), Err(KError::InvalidInput)),
            Ordering::Release,
        );
    }
    let _wake_gate = ScopedDaemonWakeGate::disabled();
    let _action = ScopedSoftirqAction::install(SoftirqVec::Block, action);
    let _guard = NoPreemptIrqSave::new();
    REJECTED.store(false, Ordering::Release);
    kirq::softirq::raise_softirq(SoftirqVec::Block);
    kirq::softirq::run_pending_softirqs();
    assert!(REJECTED.load(Ordering::Acquire));
}

#[def_test(serial)]
fn prepared_wait_rechecks_after_spurious_and_repeated_wakes() {
    let published = Arc::new(SpinNoIrq::new(None::<Waker>));
    let signal = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(AtomicUsize::new(0));
    let park_returns = Arc::new(AtomicUsize::new(0));
    let task = crate::spawn({
        let published = published.clone();
        let signal = signal.clone();
        let observed = observed.clone();
        let park_returns = park_returns.clone();
        move || {
            let mut wait = PreparedTaskWait::prepare().unwrap();
            *published.lock() = Some(wait.waker());
            for round in 1..=2 {
                while signal.load(Ordering::Acquire) < round {
                    wait.park();
                    park_returns.fetch_add(1, Ordering::Release);
                }
                observed.store(round, Ordering::Release);
            }
        }
    });
    while task.state() != crate::TaskState::Blocked {
        crate::yield_now();
    }
    let waker = published.lock().as_ref().unwrap().clone();
    // A scheduler wake without a KWaker hint must still return to the predicate.
    select_wake_run_queue::<NoPreemptIrqSave>(&task).unblock_task(task.clone(), true);
    while park_returns.load(Ordering::Acquire) == 0 || task.state() != crate::TaskState::Blocked {
        crate::yield_now();
    }
    let before_signal = observed.load(Ordering::Acquire);
    signal.store(1, Ordering::Release);
    waker.wake_by_ref();
    while observed.load(Ordering::Acquire) < 1 || task.state() != crate::TaskState::Blocked {
        crate::yield_now();
    }
    signal.store(2, Ordering::Release);
    waker.wake_by_ref();
    let exit = task.join();
    assert_eq!(exit, 0);
    assert_eq!(before_signal, 0);
    assert_eq!(observed.load(Ordering::Acquire), 2);
}

#[def_test(serial)]
fn block_on_repolls_after_early_wake() {
    let mut polls = 0;
    let result = block_on(poll_fn(|cx| {
        polls += 1;
        if polls == 1 {
            cx.waker().wake_by_ref();
            Poll::Pending
        } else {
            Poll::Ready(42)
        }
    }));
    assert_eq!(result, 42);
    assert_eq!(polls, 2);
}

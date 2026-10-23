// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{
    mem::size_of,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    task::Waker,
};

use kspin::{NoPreemptIrqSave, SpinNoIrq};
use unittest::{assert, assert_eq, def_test};

use super::*;

#[def_test(serial)]
fn block_wait_preserves_early_signals_and_independent_sources() {
    let prepare: block::completion::PrepareBlockWait = prepare_block_wait;
    let mut waiter = prepare().unwrap();
    let signals = waiter.signals();
    assert!(Arc::ptr_eq(&signals, &waiter.signals()));
    signals.notify_admission();
    signals.notify_admission();
    waiter.wait_admission().unwrap();
    waiter.wait_admission().unwrap();
    signals.notify_completion();
    waiter.wait_completion();
    waiter.wait_completion();
    drop(waiter);
    signals.notify_completion();

    let mut waiter = HostBlockWaiter::prepare().unwrap();
    for _ in 0..3 {
        waiter.signals.notify_admission();
        waiter.wait_admission().unwrap();
        assert!(!waiter.signals.admission.try_wait());
        assert!(!waiter.signals.terminal.is_completed());
    }
    assert_eq!(waiter.signals.terminal.complete_all(), 1);
    waiter.wait_completion();
    unittest::ktest_println!(
        "block wait metadata: waiter={} signals={} task_wait={} registration={} completion={}",
        size_of::<HostBlockWaiter>(),
        size_of::<HostSignals>(),
        size_of::<PreparedTaskWait>(),
        size_of::<PollRegistration>(),
        size_of::<Completion>(),
    );
}

#[def_test(serial)]
fn block_signals_outlive_waiter_and_cancel_terminal_registration() {
    let waiter = HostBlockWaiter::prepare().unwrap();
    let signals = waiter.signals.clone();
    drop(waiter);
    // No surviving source registration may retain the retired waiting task.
    assert_eq!(signals.terminal.complete_all(), 0);
    let _guard = NoPreemptIrqSave::new();
    signals.notify_admission();
    signals.notify_completion();
    signals.notify_completion();
    assert!(signals.admission.try_wait());
    assert!(signals.terminal.try_wait());
    assert!(signals.terminal.try_wait());
}

#[def_test(serial)]
fn block_wait_rejects_atomic_context_and_maps_registration_errors() {
    let _guard = NoPreemptIrqSave::new();
    assert!(matches!(
        prepare_block_wait(),
        Err(DriverError::InvalidInput)
    ));
    assert_eq!(
        registration_error(PollRegisterError::NoMemory),
        DriverError::NoMemory
    );
    assert_eq!(
        registration_error(PollRegisterError::IdExhausted),
        DriverError::NoMemory
    );
    assert_eq!(
        registration_error(PollRegisterError::InvalidState),
        DriverError::InvalidInput
    );
}

#[def_test(serial)]
fn block_terminal_registration_survives_admission_and_spurious_wake() {
    let published = Arc::new(SpinNoIrq::new(None::<(Arc<HostSignals>, Waker)>));
    let admissions = Arc::new(AtomicUsize::new(0));
    let finished = Arc::new(AtomicBool::new(false));
    let task = ktask::spawn({
        let published = published.clone();
        let admissions = admissions.clone();
        let finished = finished.clone();
        move || {
            let mut waiter = HostBlockWaiter::prepare().unwrap();
            *published.lock() = Some((waiter.signals.clone(), waiter.task.waker()));
            for round in 1..=2 {
                waiter.wait_admission().unwrap();
                admissions.store(round, Ordering::Release);
            }
            waiter.wait_completion();
            finished.store(true, Ordering::Release);
        }
    });
    while task.state() != ktask::TaskState::Blocked {
        ktask::yield_now();
    }
    let (signals, waker) = published.lock().as_ref().unwrap().clone();
    for round in 1..=2 {
        {
            let _guard = NoPreemptIrqSave::new();
            signals.notify_admission();
        }
        while admissions.load(Ordering::Acquire) < round
            || task.state() != ktask::TaskState::Blocked
        {
            ktask::yield_now();
        }
    }
    // Wake synchronously makes the blocked task runnable; wait for its next
    // park without ever publishing a terminal completion.
    waker.wake_by_ref();
    while task.state() != ktask::TaskState::Blocked {
        ktask::yield_now();
    }
    let returned_early = finished.load(Ordering::Acquire);
    let notified = signals.terminal.complete_all();
    let exit = task.join();
    assert!(!returned_early);
    assert_eq!(notified, 1);
    assert_eq!(exit, 0);
    assert!(finished.load(Ordering::Acquire));
}

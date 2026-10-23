// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::mem::size_of;

use kspin::NoPreemptIrqSave;
use unittest::{assert, assert_eq, def_test};

use super::*;

#[def_test(serial)]
fn block_wait_preserves_early_terminal_signals() {
    let mut waiter = prepare_block_wait().unwrap();
    let signals = waiter.signals();
    assert!(Arc::ptr_eq(&signals, &waiter.signals()));
    signals.notify_completion();
    waiter.wait_completion();
    waiter.wait_completion();
    drop(waiter);
    signals.notify_completion();

    let mut waiter = HostBlockWaiter::prepare().unwrap();
    assert_eq!(waiter.signals.terminal.complete_all(), 0);
    waiter.wait_completion();
    unittest::ktest_println!(
        "block wait metadata: waiter={} signals={} completion={}",
        size_of::<HostBlockWaiter>(),
        size_of::<HostSignals>(),
        size_of::<Completion>(),
    );
}

#[def_test(serial)]
fn block_signals_outlive_waiter_without_task_registration() {
    let waiter = HostBlockWaiter::prepare().unwrap();
    let signals = waiter.signals.clone();
    drop(waiter);
    // No surviving source registration may retain the retired waiting task.
    assert_eq!(signals.terminal.complete_all(), 0);
    let _guard = NoPreemptIrqSave::new();
    signals.notify_completion();
    signals.notify_completion();
    assert!(signals.terminal.try_wait());
    assert!(signals.terminal.try_wait());
}

#[def_test(serial)]
fn block_wait_rejects_atomic_context_before_submission() {
    let _guard = NoPreemptIrqSave::new();
    assert!(matches!(
        prepare_block_wait(),
        Err(DriverError::InvalidInput)
    ));
}

#[def_test(serial)]
fn block_terminal_wait_survives_spurious_wake() {
    let mut waiter = HostBlockWaiter::prepare().unwrap();
    let signals = waiter.signals.clone();
    let mut parks = 0;
    let mut notified = 0;
    waiter.wait_completion_before_park(|task| {
        parks += 1;
        if parks == 1 {
            // A wake hint without a terminal result must return to the predicate.
            // Latch it before park so the test needs no scheduler-state polling.
            task.waker().wake_by_ref();
        } else {
            // The same completion registration must survive the spurious wake.
            notified = signals.terminal.complete_all();
        }
    });
    assert_eq!(parks, 2);
    assert_eq!(notified, 1);
    assert!(signals.terminal.try_wait());
    assert_eq!(signals.terminal.complete_all(), 0);
}

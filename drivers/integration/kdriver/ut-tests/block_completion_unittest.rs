// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::mem::size_of;

use kspin::NoPreemptIrqSave;
use unittest::{assert, assert_eq, def_test};

use super::*;

#[def_test(serial)]
#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
)]
#[allow(
    unknown_lints,
    std_box_in_kernel_tests,
    reason = "CG-049-no-std-box-in-kernel-tests: prepare_block_wait returns the production \
              Box<dyn BlockWaiter> contract; this test retains that owner through terminal \
              signaling or reclaimer shutdown"
)]
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
fn block_terminal_wait_observes_cross_task_completion() {
    let mut waiter = HostBlockWaiter::prepare().unwrap();
    let signals = waiter.signals.clone();
    let producer = ktask::spawn(move || {
        signals.terminal.complete_all();
    });
    waiter.wait_completion();
    assert_eq!(producer.join(), 0);
    assert!(waiter.signals.terminal.try_wait());
    assert_eq!(waiter.signals.terminal.complete_all(), 0);
}

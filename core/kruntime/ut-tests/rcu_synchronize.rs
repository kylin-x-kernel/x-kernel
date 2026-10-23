// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Public RCU wait semantics against boot's permanent task and callback services.
//! Tests do not replace the service or rely on another test to initialize it.

use alloc::boxed::Box;

use kerrno::KError;
use krcu::{Rcu, synchronize_rcu};
use unittest::{assert, assert_eq, def_test};

#[def_test]
fn public_scope_rejects_wait_and_protects_multiple_containers() {
    let left = Rcu::new(Box::new(11));
    let right = Rcu::new(Box::new(22));
    let (sum, wait_result) = krcu::with_read(|scope| {
        let sum = *left.read(scope) + *right.read(scope);
        // SAFETY: no data is retired; this checks rejection before any marker
        // is submitted. InvalidInput must not be interpreted as GP completion.
        let result = unsafe { synchronize_rcu() };
        (sum, result)
    });
    assert_eq!(sum, 33);
    assert_eq!(wait_result, Err(KError::InvalidInput));
    // SAFETY: the scope ended, no reader-needed lock or resource is held,
    // and the boot service remains available. No data is retired by this test.
    assert_eq!(unsafe { synchronize_rcu() }, Ok(()));
}

#[def_test(serial)]
fn synchronous_wait_uses_the_live_callback_service() {
    for _ in 0..4 {
        // SAFETY: no object is retired by this fixture and no read scope or
        // lock is held. Boot established the permanent participating services.
        assert_eq!(unsafe { synchronize_rcu() }, Ok(()));
    }
}

#[def_test(serial)]
fn pending_interrupt_does_not_cancel_synchronous_wait() {
    let current = ktask::current();
    let was_interrupted = current.is_interrupted();
    current.interrupt();
    // SAFETY: this task-only fixture retires no object and holds no read scope
    // or lock. Boot retains the participating services throughout the wait.
    let result = unsafe { synchronize_rcu() };
    let is_still_interrupted = current.is_interrupted();
    if !was_interrupted {
        current.clear_interrupt();
    }
    assert_eq!(result, Ok(()));
    assert!(is_still_interrupted);
}

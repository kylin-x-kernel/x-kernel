// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use unittest::{assert, assert_eq, def_test};

use super::*;

#[def_test(serial)]
fn test_kirq_sync_wait_provider_blocks_until_completion() {
    static WAITER_STARTED: AtomicUsize = AtomicUsize::new(0);
    static WAITER_FINISHED: AtomicUsize = AtomicUsize::new(0);

    WAITER_STARTED.store(0, Ordering::Release);
    WAITER_FINISHED.store(0, Ordering::Release);

    let completion = Arc::new(Completion::new());
    let waiter_completion = completion.clone();
    assert!(kirq::IrqSyncWaitIf::can_wait());

    crate::spawn(move || {
        WAITER_STARTED.store(1, Ordering::Release);
        kirq::IrqSyncWaitIf::wait_for_completion(&waiter_completion).unwrap();
        WAITER_FINISHED.store(1, Ordering::Release);
    });

    while WAITER_STARTED.load(Ordering::Acquire) == 0 {
        crate::yield_now();
    }
    crate::yield_now();
    assert_eq!(WAITER_FINISHED.load(Ordering::Acquire), 0);

    completion.complete_all();
    while WAITER_FINISHED.load(Ordering::Acquire) == 0 {
        crate::yield_now();
    }
    assert!(completion.is_completed());
}

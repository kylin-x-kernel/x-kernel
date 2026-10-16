// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::task::Waker;

use unittest::def_test;

use super::{BucketId, FutexWaiter};
use crate::FutexKey;

#[def_test]
fn waiter_route_generation_changes_on_requeue() {
    let source = FutexKey::private_for_test(1, 0x1000);
    let target = FutexKey::private_for_test(1, 0x2000);
    let waiter = Arc::new(FutexWaiter::new(
        source,
        BucketId(3),
        u32::MAX,
        Waker::noop().clone(),
    ));
    waiter.mark_queued();
    assert_eq!(waiter.route(), (source, BucketId(3), 0));

    waiter.requeue_to(target, BucketId(7));
    assert_eq!(waiter.route(), (target, BucketId(7), 1));
    assert!(waiter.try_cancel());
    assert!(!waiter.try_wake());
}

#[def_test]
fn waiter_has_single_terminal_transition() {
    let key = FutexKey::private_for_test(2, 0x3000);
    let waiter = FutexWaiter::new(key, BucketId(1), 0x10, Waker::noop().clone());
    waiter.mark_queued();

    assert!(waiter.try_wake());
    assert!(waiter.is_woken());
    assert!(!waiter.try_cancel());
}

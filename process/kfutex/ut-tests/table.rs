// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::task::Waker;

use unittest::def_test;

use super::FutexTable;
use crate::{FutexKey, waiter::FutexWaiter};

fn queued_waiter(key: FutexKey, table: &FutexTable) -> Arc<FutexWaiter> {
    let bucket = table.bucket_id(key);
    Arc::new(FutexWaiter::new(
        key,
        bucket,
        u32::MAX,
        Waker::noop().clone(),
    ))
}

#[def_test]
fn requeue_same_key_wakes_and_keeps_waiters() {
    let table = FutexTable::new();
    let key = FutexKey::private_for_test(1, 0x1000);
    let first = queued_waiter(key, &table);
    let second = queued_waiter(key, &table);
    table.enqueue_waiter_for_test(key, first.clone());
    table.enqueue_waiter_for_test(key, second.clone());

    let count = table
        .requeue(key, key, 1, 1, None)
        .expect("same-key requeue is valid");
    assert_eq!(count, 2);
    assert!(first.is_woken() ^ second.is_woken());
    let kept = if first.is_woken() { &second } else { &first };
    assert_eq!(kept.route().0, key);
    assert!(kept.is_queued());
}

#[def_test]
fn requeue_wakes_one_and_moves_one() {
    let table = FutexTable::new();
    let source = FutexKey::private_for_test(10, 0x1000);
    let target = FutexKey::private_for_test(10, 0x2000);
    let first = queued_waiter(source, &table);
    let second = queued_waiter(source, &table);
    table.enqueue_waiter_for_test(source, first.clone());
    table.enqueue_waiter_for_test(source, second.clone());

    let count = table
        .requeue(source, target, 1, 1, None)
        .expect("requeue waiters");
    assert_eq!(count, 2);
    assert!(first.is_woken() ^ second.is_woken());
    let moved = if first.is_woken() { &second } else { &first };
    assert_eq!(moved.route().0, target);
    assert!(moved.is_queued());
}

#[def_test]
fn cancel_removes_requeued_waiter() {
    let table = FutexTable::new();
    let source = FutexKey::private_for_test(11, 0x3000);
    let target = FutexKey::private_for_test(11, 0x4000);
    let waiter = queued_waiter(source, &table);
    table.enqueue_waiter_for_test(source, waiter.clone());

    table
        .requeue(source, target, 0, 1, None)
        .expect("requeue waiter to target");
    assert_eq!(waiter.route().0, target);
    assert!(waiter.is_queued());

    assert!(table.cancel_waiter_for_test(&waiter));
    assert!(!waiter.is_queued());
    assert!(!waiter.try_cancel());
}

#[def_test]
fn wake_wins_over_late_cancellation() {
    let table = FutexTable::new();
    let key = FutexKey::private_for_test(12, 0x5000);
    let waiter = queued_waiter(key, &table);
    table.enqueue_waiter_for_test(key, waiter.clone());

    assert_eq!(table.wake(key, 1, u32::MAX), 1);
    assert!(!table.cancel_waiter_for_test(&waiter));
    assert!(waiter.is_woken());
}

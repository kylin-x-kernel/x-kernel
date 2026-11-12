// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::task::Waker;

use kerrno::{KError, KErrorKind};
use ktask::{KtaskRef, RtPrio, SchedAttr, SchedPolicy, TaskInner, TaskState};
use unittest::{assert, assert_eq, def_test};

use super::{FUTEX_TABLE, FutexTable, PiLock};
use crate::{FutexKey, global_table, waiter::FutexWaiter};

fn key_for(id: usize) -> FutexKey {
    FutexKey::private_for_test(0x5049_7465_7374, id * 4)
}

fn rt_worker(body: impl FnOnce() + Send + 'static) -> KtaskRef {
    let task = ktask::prepare_task(TaskInner::new_pidless_kthread(
        body,
        "futex-pi-test".into(),
        256 * 1024,
    ));
    ktask::init_task_sched_attr(
        &task,
        SchedAttr {
            policy: SchedPolicy::Fifo,
            rt_priority: RtPrio::new(30).unwrap(),
            ..SchedAttr::DEFAULT
        },
    )
    .unwrap();
    let mut mask = ktask::KCpuMask::new();
    mask.set(khal::percpu::this_cpu_id().as_usize(), true);
    task.set_cpumask(mask);
    task
}

#[def_test]
fn commands_retain_ownership_and_reject_busy_or_foreign_release() {
    let key = key_for(1);
    let table = global_table();
    let owner = ktask::current().clone();
    table.trylock_pi(key, None).unwrap();
    let worker = ktask::spawn(move || {
        core::assert_eq!(table.trylock_pi(key, None), Err(KError::WouldBlock));
        core::assert_eq!(
            table.unlock_pi(key, &ktask::current().clone()),
            Err(KError::OperationNotPermitted)
        );
    });
    let status = worker.join();
    let recursive = table.lock_pi(key, None, None);
    table.unlock_pi(key, &owner).unwrap();
    assert_eq!(status, 0);
    assert_eq!(recursive, Err(KError::from(KErrorKind::Deadlock)));
    assert!(
        !FUTEX_TABLE
            .bucket(FUTEX_TABLE.bucket_id(key))
            .state
            .lock()
            .pi
            .contains_key(&key.pi_index())
    );
}

#[def_test]
fn rt_waiter_donates_and_handoff_restores_the_previous_owner() {
    let key = key_for(2);
    let table = global_table();
    let owner = ktask::current().clone();
    table.trylock_pi(key, None).unwrap();
    let worker = rt_worker(move || {
        table.lock_pi(key, None, None).unwrap();
        table.unlock_pi(key, &ktask::current().clone()).unwrap();
    });
    ktask::activate_task(&worker);
    while !matches!(worker.state(), TaskState::Blocked | TaskState::Exited) {
        ktask::yield_now();
    }
    let was_blocked = worker.state() == TaskState::Blocked;
    let boost = owner.pi_donor_priority_snapshot();
    table.unlock_pi(key, &owner).unwrap();
    let status = worker.join();
    assert!(was_blocked);
    assert_eq!(boost, 30);
    assert_eq!(status, 0);
    assert_eq!(owner.pi_donor_priority_snapshot(), 0);
}

#[def_test]
fn timeout_withdraws_donation_from_an_adopted_owner() {
    let key = key_for(3);
    let owner = ktask::current().clone();
    let original_owner = owner.clone();
    let worker = rt_worker(move || {
        core::assert_eq!(
            global_table().lock_pi(key, Some(original_owner), Some(ktime_types::TimeSpan::ZERO)),
            Err(KError::TimedOut)
        );
    });
    ktask::activate_task(&worker);
    let status = worker.join();
    let boost = owner.pi_donor_priority_snapshot();
    global_table().unlock_pi(key, &owner).unwrap();
    assert_eq!(status, 0);
    assert_eq!(boost, 0);
}

#[def_test]
fn pi_and_ordinary_waiters_cannot_share_a_key() {
    let table = FutexTable::new();
    let key = key_for(4);
    let other = key_for(5);
    let waiter = Arc::new(FutexWaiter::new(
        key,
        table.bucket_id(key),
        u32::MAX,
        Waker::noop().clone(),
    ));
    table.enqueue_waiter_for_test(key, waiter.clone());
    assert_eq!(table.trylock_pi(key, None), Err(KError::InvalidInput));
    assert!(table.cancel_waiter_for_test(&waiter));
    table.trylock_pi(key, None).unwrap();
    let from_pi = table.requeue(key, other, 0, 1, None);
    let to_pi = table.requeue(other, key, 0, 1, None);
    let ordinary_wake = table.wake(key, 1, u32::MAX);
    table.unlock_pi(key, &ktask::current().clone()).unwrap();
    assert_eq!(from_pi, Err(KError::InvalidInput));
    assert_eq!(to_pi, Err(KError::InvalidInput));
    assert_eq!(ordinary_wake, 0);
}

#[def_test]
fn raw_cancellation_preserves_a_grant_and_withdraws_an_ungranted_wait() {
    let owner = ktask::current().clone();
    let lock = Arc::new(PiLock::new(Some(owner.clone())));
    let worker_lock = lock.clone();
    let worker = ktask::spawn(move || {
        core::assert!(worker_lock.begin_lock().unwrap().cancel());
        let wait = worker_lock.begin_lock().unwrap();
        core::assert!(
            worker_lock
                .unlock_deferred(&owner, |_| Err(KError::BadAddress))
                .is_err()
        );
        core::assert!(worker_lock.is_owned_by(&owner));
        worker_lock.unlock(&owner).unwrap();
        core::assert!(!wait.cancel());
        worker_lock.unlock(&ktask::current().clone()).unwrap();
    });
    let status = worker.join();
    assert_eq!(status, 0);
    assert!(!lock.is_locked());
}

#[def_test]
fn an_inflight_lookup_keeps_a_free_key_bound_to_the_same_pi_lock() {
    let table = FutexTable::new();
    let key = key_for(6);
    let pin = table.pi_lock(key, None).unwrap();
    table.trylock_pi(key, None).unwrap();
    table.unlock_pi(key, &ktask::current().clone()).unwrap();
    let next = table.pi_lock(key, None).unwrap();
    assert!(Arc::ptr_eq(&pin, &next));
    table.retire_pi(key, pin);
    table.retire_pi(key, next);
    assert!(
        table
            .bucket(table.bucket_id(key))
            .state
            .lock()
            .pi
            .is_empty()
    );
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Live PI integration with explicit contention and joined worker lifetimes.
//! State waits have no timing assertion; the QEMU harness bounds a hang.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use kcpu_id_map::KCpuMaskExt;
use unittest::{assert, assert_eq, def_test};

use super::TaskPiHost;
use crate::{KtaskRef, RtPrio, SchedAttr, SchedPolicy, TaskInner, TaskState};

type Mutex<T> = krtmutex::RtMutex<TaskPiHost, T>;

fn fifo(priority: u8) -> SchedAttr {
    SchedAttr {
        policy: SchedPolicy::Fifo,
        rt_priority: RtPrio::new(i32::from(priority)).expect("test RT priority"),
        ..SchedAttr::DEFAULT
    }
}

fn prepare_worker(body: impl FnOnce() + Send + 'static, attr: SchedAttr) -> KtaskRef {
    let task = crate::prepare_task(TaskInner::new_pidless_kthread_with_entry(
        crate::PreparedTaskEntry::new_nowait(body).unwrap(),
        "pi-test".into(),
        kbuild_config::TASK_STACK_SIZE,
    ));
    crate::init_task_sched_attr(&task, attr).expect("valid test scheduling attributes");
    task.set_cpumask(crate::KCpuMask::one_shot_logical(
        khal::percpu::this_cpu_id(),
    ));
    task
}

fn wait_until_blocked(task: &KtaskRef) -> bool {
    loop {
        match task.state() {
            TaskState::Blocked => return true,
            TaskState::Exited => return false,
            _ => crate::yield_now(),
        }
    }
}

fn set_attr(task: &KtaskRef, attr: SchedAttr) {
    crate::sched_update_attr(task, |live| {
        *live = attr;
        Ok::<_, core::convert::Infallible>(())
    })
    .ok()
    .expect("valid test scheduling update");
}

#[def_test]
fn inheritance_and_policy_changes_propagate_through_a_blocked_owner() {
    let first_lock = Arc::new(Mutex::new(()));
    let second_lock = Arc::new(Mutex::new(()));
    let guard = first_lock.lock().unwrap();
    let middle = prepare_worker(
        {
            let first_lock = first_lock.clone();
            let second_lock = second_lock.clone();
            move || {
                let _second = second_lock.lock().unwrap();
                let _first = first_lock.lock().unwrap();
            }
        },
        SchedAttr::DEFAULT,
    );
    crate::activate_task(&middle);
    let middle_blocked = wait_until_blocked(&middle);
    let high = prepare_worker(
        move || {
            let _guard = second_lock.lock().unwrap();
        },
        fifo(30),
    );
    crate::activate_task(&high);
    let high_blocked = wait_until_blocked(&high);
    let boosted = (
        middle.pi_donor_priority_snapshot(),
        crate::current().pi_donor_priority_snapshot(),
    );
    set_attr(&high, fifo(10));
    let lowered = (
        middle.pi_donor_priority_snapshot(),
        crate::current().pi_donor_priority_snapshot(),
    );
    drop(guard);
    let statuses = [middle.join(), high.join()];
    assert!(middle_blocked && high_blocked);
    assert_eq!(boosted, (30, 30));
    assert_eq!(lowered, (10, 10));
    assert_eq!(statuses, [0, 0]);
    assert_eq!(middle.pi_donor_priority_snapshot(), 0);
    assert_eq!(crate::current().pi_donor_priority_snapshot(), 0);
}

#[def_test]
fn pi_handoff_wakes_high_waiter_before_medium_priority_peer_can_run() {
    let driver = crate::spawn(|| {
        let cpu = khal::percpu::this_cpu_id();
        core::assert!(crate::set_current_affinity(
            crate::KCpuMask::one_shot_logical(cpu)
        ));
        let lock = Arc::new(Mutex::new(()));
        let held = Arc::new(kcompletion::Completion::new());
        let release = Arc::new(kcompletion::Completion::new());
        let sequence = Arc::new(AtomicUsize::new(0));
        let high_order = Arc::new(AtomicUsize::new(usize::MAX));
        let medium_order = Arc::new(AtomicUsize::new(usize::MAX));
        let low = prepare_worker(
            {
                let lock = lock.clone();
                let held = held.clone();
                let release = release.clone();
                let sequence = sequence.clone();
                move || {
                    let guard = lock.lock().unwrap();
                    held.complete_all();
                    crate::completion::wait_for_completion(&release).unwrap();
                    sequence.fetch_add(1, Ordering::Relaxed);
                    drop(guard);
                }
            },
            SchedAttr::DEFAULT,
        );
        let high = prepare_worker(
            {
                let sequence = sequence.clone();
                let order = high_order.clone();
                move || {
                    let _guard = lock.lock().unwrap();
                    order.store(sequence.fetch_add(1, Ordering::Relaxed), Ordering::Release);
                }
            },
            fifo(30),
        );
        let medium = prepare_worker(
            {
                let sequence = sequence.clone();
                let order = medium_order.clone();
                move || {
                    order.store(sequence.fetch_add(1, Ordering::Relaxed), Ordering::Release);
                }
            },
            fifo(20),
        );
        crate::activate_task(&low);
        crate::completion::wait_for_completion(&held).unwrap();
        crate::activate_task(&high);
        core::assert!(wait_until_blocked(&high));
        let boost = low.pi_donor_priority_snapshot();
        {
            // Both contenders become runnable on this CPU before dispatch.
            let _preempt = kspin::NoPreempt::new();
            crate::activate_task(&medium);
            release.complete_all();
        }
        let statuses = [low.join(), high.join(), medium.join()];
        core::assert_eq!(boost, 30);
        core::assert_eq!(statuses, [0, 0, 0]);
        core::assert_eq!(high_order.load(Ordering::Acquire), 1);
        core::assert_eq!(medium_order.load(Ordering::Acquire), 2);
    });
    assert_eq!(driver.join(), 0);
}

#[cfg(feature = "smp")]
#[def_test]
fn boosted_owner_migrates_and_releases_on_its_new_run_queue() {
    use kcpu_id_map::LogicalCpuId;

    if kcpu_id_map::nr_cpus() < 2 {
        return unittest::TestResult::Ignored;
    }
    let driver = crate::spawn(|| {
        let source = LogicalCpuId::new(0);
        let target = LogicalCpuId::new(1);
        core::assert!(crate::set_current_affinity(
            crate::KCpuMask::one_shot_logical(source)
        ));
        let lock = Arc::new(Mutex::new(0));
        let mut guard = lock.lock().unwrap();
        let waiter_lock = lock.clone();
        let waiter = prepare_worker(move || *waiter_lock.lock().unwrap() += 1, fifo(30));
        crate::activate_task(&waiter);
        core::assert!(wait_until_blocked(&waiter));
        core::assert_eq!(crate::current().pi_donor_priority_snapshot(), 30);
        core::assert!(crate::set_current_affinity(
            crate::KCpuMask::one_shot_logical(target)
        ));
        core::assert_eq!(khal::percpu::this_cpu_id(), target);
        let update_waiter = waiter.clone();
        let updater = prepare_worker(
            move || set_attr(&update_waiter, fifo(40)),
            SchedAttr::DEFAULT,
        );
        updater.set_cpumask(crate::KCpuMask::one_shot_logical(source));
        crate::activate_task(&updater);
        // The updater runs on the old CPU and must boost this owner's new
        // RQ while it is blocked in join, without reviving the old RQ seat.
        core::assert_eq!(updater.join(), 0);
        core::assert_eq!(crate::current().pi_donor_priority_snapshot(), 40);
        *guard = 10;
        drop(guard);
        core::assert_eq!(crate::current().pi_donor_priority_snapshot(), 0);
        core::assert_eq!(waiter.join(), 0);
        core::assert_eq!(*lock.lock().unwrap(), 11);
    });
    assert_eq!(driver.join(), 0);
}

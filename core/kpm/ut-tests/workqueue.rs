// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Real private queues using permanently boot-installed worker/timer services.
//! Each fixture thaws, drains handles and closes its queues; no registry is replaced.

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicUsize, Ordering};

use kworkng::{CpuPolicy, QueueConfig, WorkHandle};
use unittest::{assert, assert_eq, def_test};

use super::*;

fn queue(name: &str) -> NormalQueue {
    let mut cpus = Vec::new();
    kcpu_id_map::for_each_present_logical_cpu(|_, cpu, _| cpus.push(cpu));
    NormalQueue::new(
        name,
        QueueConfig::new(&cpus, CpuPolicy::CurrentCpu, 1).unwrap(),
    )
    .unwrap()
}

#[def_test(serial)]
fn queue_set_holds_pending_work_while_unselected_queue_progresses_then_thaws() {
    let queues = [queue("pm-selected0"), queue("pm-selected1")];
    let peer = queue("pm-unselected");
    let cancellation = QueueFreezeCancellation::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let first_calls = calls.clone();
    let first = WorkHandle::new(move || {
        first_calls.fetch_add(1, Ordering::Release);
    })
    .unwrap();
    let second_calls = calls.clone();
    let second = WorkHandle::new(move || {
        second_calls.fetch_add(1, Ordering::Release);
    })
    .unwrap();
    let peer_calls = Arc::new(AtomicUsize::new(0));
    let callback_peer_calls = peer_calls.clone();
    let peer_work = WorkHandle::new(move || {
        callback_peer_calls.fetch_add(1, Ordering::Release);
    })
    .unwrap();
    let mut set = WorkqueueFreezeSession::new(SuspendWorkqueues {
        queues: &queues,
        timeout: TimeSpan::from_secs(5),
        cancellation: &cancellation,
    });
    set.freeze().unwrap();
    first.queue(&queues[0], None).unwrap();
    second.queue(&queues[1], None).unwrap();
    peer_work.queue(&peer, None).unwrap();
    peer.flush().unwrap();
    assert_eq!(peer_calls.load(Ordering::Acquire), 1);
    assert_eq!(calls.load(Ordering::Acquire), 0);
    cancellation.cancel(); // A completed freeze retains ownership through recovery.
    assert_eq!(calls.load(Ordering::Acquire), 0);
    set.thaw();
    set.thaw();
    for queue in &queues {
        queue.flush().unwrap();
    }
    assert_eq!(calls.load(Ordering::Acquire), 2);
    first.shutdown().unwrap();
    second.shutdown().unwrap();
    peer_work.shutdown().unwrap();
    for queue in &queues {
        queue.close_and_drain().unwrap();
    }
    peer.close_and_drain().unwrap();
}

#[def_test(serial)]
fn busy_queue_retains_successful_prefix_until_explicit_cleanup_without_stealing_owner() {
    let queues = [queue("pm-prefix"), queue("pm-busy")];
    let cancellation = QueueFreezeCancellation::new();
    let external = queues[1].freeze(TimeSpan::ZERO, &cancellation).unwrap();
    let mut set = WorkqueueFreezeSession::new(SuspendWorkqueues {
        queues: &queues,
        timeout: TimeSpan::ZERO,
        cancellation: &cancellation,
    });
    assert_eq!(
        set.freeze(),
        Err(WorkqueueFreezeError {
            index: 1,
            error: QueueFreezeError::Busy
        })
    );
    assert!(matches!(
        queues[0].freeze(TimeSpan::ZERO, &cancellation),
        Err(QueueFreezeError::Busy)
    ));
    set.thaw();
    drop(queues[0].freeze(TimeSpan::ZERO, &cancellation).unwrap());
    assert!(matches!(
        queues[1].freeze(TimeSpan::ZERO, &cancellation),
        Err(QueueFreezeError::Busy)
    ));
    drop(external);
    set.freeze().unwrap();
    set.thaw();
    for queue in &queues {
        queue.close_and_drain().unwrap();
    }
}

#[def_test(serial)]
fn duplicate_queue_alias_fails_at_later_position_and_releases_original_pause() {
    let original = queue("pm-duplicate");
    let queues = [original.clone(), original.clone()];
    let cancellation = QueueFreezeCancellation::new();
    let mut set = WorkqueueFreezeSession::new(SuspendWorkqueues {
        queues: &queues,
        timeout: TimeSpan::ZERO,
        cancellation: &cancellation,
    });
    assert_eq!(
        set.freeze(),
        Err(WorkqueueFreezeError {
            index: 1,
            error: QueueFreezeError::Busy
        })
    );
    set.thaw();
    drop(original.freeze(TimeSpan::ZERO, &cancellation).unwrap());
    original.close_and_drain().unwrap();
}

#[def_test(serial)]
fn cancelled_or_closed_queue_set_can_be_cleaned_without_leaving_pauses() {
    let queues = [queue("pm-cancel"), queue("pm-closed")];
    let cancelled = QueueFreezeCancellation::new();
    cancelled.cancel();
    let mut set = WorkqueueFreezeSession::new(SuspendWorkqueues {
        queues: &queues,
        timeout: TimeSpan::ZERO,
        cancellation: &cancelled,
    });
    assert_eq!(
        set.freeze(),
        Err(WorkqueueFreezeError {
            index: 0,
            error: QueueFreezeError::Cancelled
        })
    );
    set.thaw();
    let fresh = QueueFreezeCancellation::new();
    queues[1].close_and_drain().unwrap();
    let mut retry = WorkqueueFreezeSession::new(SuspendWorkqueues {
        queues: &queues,
        timeout: TimeSpan::ZERO,
        cancellation: &fresh,
    });
    assert_eq!(
        retry.freeze(),
        Err(WorkqueueFreezeError {
            index: 1,
            error: QueueFreezeError::Closed
        })
    );
    retry.thaw();
    drop(queues[0].freeze(TimeSpan::ZERO, &fresh).unwrap());
    queues[0].close_and_drain().unwrap();
}

/// Uses the production system task snapshot, real remote tasks and real queues.
/// All boot infrastructure is explicitly exempted only inside this serial test.
#[def_test(serial)]
fn smp_preparation_holds_tasks_and_callbacks_until_simulated_device_recovery() {
    use core::sync::atomic::AtomicBool;

    use kcpu_id_map::{KCpuMask, KCpuMaskExt};

    let mut cpus = Vec::new();
    kcpu_id_map::for_each_present_logical_cpu(|_, cpu, _| cpus.push(cpu));
    let device_suspended = Arc::new(AtomicBool::new(false));
    let mut fixtures = Vec::new();
    let mut queues = Vec::new();
    for cpu in cpus {
        let queue = NormalQueue::new(
            "pm-smp-integration",
            QueueConfig::new(&[cpu], CpuPolicy::RoundRobin, 1).unwrap(),
        )
        .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let callback_calls = calls.clone();
        let suspended = device_suspended.clone();
        let work = WorkHandle::new(move || {
            core::assert!(!suspended.load(Ordering::Acquire));
            core::assert_eq!(khal::percpu::this_cpu_id(), cpu);
            callback_calls.fetch_add(1, Ordering::Release);
        })
        .unwrap();
        work.queue(&queue, Some(cpu)).unwrap();
        queue.flush().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let visits = Arc::new(AtomicUsize::new(0));
        let pending = Arc::new(AtomicBool::new(false));
        let wake_slot = Arc::new(kspin::SpinNoIrq::new(None));
        let worker_stop = stop.clone();
        let worker_visits = visits.clone();
        let worker_pending = pending.clone();
        let worker_slot = wake_slot.clone();
        let task = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread(
            move || {
                core::assert_eq!(khal::percpu::this_cpu_id(), cpu);
                let mut wait = ktask::future::PreparedTaskWait::prepare().unwrap();
                *worker_slot.lock() = Some(wait.waker());
                while !worker_stop.load(Ordering::Acquire) {
                    wait.park_freezable().unwrap();
                    if worker_pending.swap(false, Ordering::AcqRel) {
                        worker_visits.fetch_add(1, Ordering::Release);
                    }
                }
            },
            "pm-smp-participant".into(),
            0x10000,
        ));
        task.set_cpumask(KCpuMask::one_shot_logical(cpu));
        ktask::activate_task(&task);
        while task.state() != ktask::TaskState::Blocked {
            ktask::yield_now();
        }
        queues.push(queue);
        fixtures.push((cpu, work, calls, task, stop, visits, wake_slot, pending));
    }
    let current = ktask::current().clone();
    let exempt: Vec<_> = ktask::freezer::test_support::admitted_tasks()
        .into_iter()
        .filter(|task| {
            !ktask::KtaskRef::ptr_eq(task, &current)
                && !fixtures.iter().any(|f| ktask::KtaskRef::ptr_eq(task, &f.3))
        })
        .collect();
    for round in 0..3 {
        let task_cancel = ktask::freezer::FreezeCancellation::new();
        let queue_cancel = QueueFreezeCancellation::new();
        let held = freeze_system_workqueues(
            crate::SuspendTasks {
                exempt: &exempt,
                timeout: TimeSpan::from_secs(30),
                cancellation: &task_cancel,
            },
            SuspendWorkqueues {
                queues: &queues,
                timeout: TimeSpan::from_secs(30),
                cancellation: &queue_cancel,
            },
        )
        .unwrap();
        // A test-owned flag models resource availability, not real device power.
        device_suspended.store(true, Ordering::Release);
        for (index, (cpu, work, calls, task, _, visits, slot, pending)) in
            fixtures.iter().enumerate()
        {
            assert_eq!(task.state(), ktask::TaskState::Frozen);
            work.queue(&queues[index], Some(*cpu)).unwrap();
            pending.store(true, Ordering::Release);
            slot.lock().as_ref().unwrap().wake_by_ref();
            ktask::wake_task(task, true);
            assert_eq!(task.state(), ktask::TaskState::Frozen);
            assert_eq!(calls.load(Ordering::Acquire), round + 1);
            assert_eq!(visits.load(Ordering::Acquire), round);
        }
        task_cancel.cancel();
        queue_cancel.cancel();
        // A second public preparation cannot consume cancelled handles or
        // release the first owner's queues/task admission while it is held.
        assert!(matches!(
            freeze_system_workqueues(
                crate::SuspendTasks {
                    exempt: &exempt,
                    timeout: TimeSpan::ZERO,
                    cancellation: &task_cancel,
                },
                SuspendWorkqueues {
                    queues: &queues,
                    timeout: TimeSpan::ZERO,
                    cancellation: &queue_cancel,
                },
            ),
            Err(crate::SuspendError::Busy)
        ));
        for (index, (_, _, calls, task, _, visits, ..)) in fixtures.iter().enumerate() {
            assert_eq!(task.state(), ktask::TaskState::Frozen);
            assert_eq!(calls.load(Ordering::Acquire), round + 1);
            assert_eq!(visits.load(Ordering::Acquire), round);
            assert!(matches!(
                queues[index].freeze(TimeSpan::ZERO, &QueueFreezeCancellation::new()),
                Err(QueueFreezeError::Busy)
            ));
        }
        let deferred_ran = Arc::new(AtomicBool::new(false));
        let ran = deferred_ran.clone();
        let deferred = ktask::spawn(move || ran.store(true, Ordering::Release));
        assert!(!deferred_ran.load(Ordering::Acquire));
        device_suspended.store(false, Ordering::Release);
        drop(held);
        for (index, (_, _, calls, task, _, visits, ..)) in fixtures.iter().enumerate() {
            queues[index].flush().unwrap();
            while visits.load(Ordering::Acquire) != round + 1
                || task.state() != ktask::TaskState::Blocked
            {
                ktask::yield_now();
            }
            assert_eq!(calls.load(Ordering::Acquire), round + 2);
        }
        assert_eq!(deferred.join(), 0);
        assert!(deferred_ran.load(Ordering::Acquire));
    }
    // A later alias fails after acquiring the first queue; cleanup must
    // release that prefix and task admission so another transaction succeeds.
    let aliases = [queues[0].clone(), queues[0].clone()];
    let task_cancel = ktask::freezer::FreezeCancellation::new();
    let queue_cancel = QueueFreezeCancellation::new();
    let result = freeze_system_workqueues(
        crate::SuspendTasks {
            exempt: &exempt,
            timeout: TimeSpan::from_secs(30),
            cancellation: &task_cancel,
        },
        SuspendWorkqueues {
            queues: &aliases,
            timeout: TimeSpan::from_secs(30),
            cancellation: &queue_cancel,
        },
    );
    assert!(matches!(
        result,
        Err(crate::SuspendError::WorkqueueFreeze(WorkqueueFreezeError {
            index: 1,
            error: QueueFreezeError::Busy
        }))
    ));
    for (_, _, _, task, _, visits, ..) in &fixtures {
        while visits.load(Ordering::Acquire) != 3 || task.state() != ktask::TaskState::Blocked {
            ktask::yield_now();
        }
    }
    let held = freeze_system_workqueues(
        crate::SuspendTasks {
            exempt: &exempt,
            timeout: TimeSpan::from_secs(30),
            cancellation: &task_cancel,
        },
        SuspendWorkqueues {
            queues: &queues,
            timeout: TimeSpan::from_secs(30),
            cancellation: &queue_cancel,
        },
    )
    .unwrap();
    drop(held);
    for (index, (_, work, _, task, stop, _, slot, _)) in fixtures.into_iter().enumerate() {
        stop.store(true, Ordering::Release);
        slot.lock().as_ref().unwrap().wake_by_ref();
        assert_eq!(task.join(), 0);
        work.shutdown().unwrap();
        queues[index].close_and_drain().unwrap();
    }
}

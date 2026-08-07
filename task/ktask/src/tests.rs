// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, Once};

use crate::{WaitQueue, api as ktask, current};

static INIT: Once = Once::new();
static SERIAL: Mutex<()> = Mutex::new(());

#[test]
fn test_sched_fifo() {
    let _lock = SERIAL.lock();
    INIT.call_once(ktask::init_scheduler);

    const NUM_TASKS: usize = 10;
    static FINISHED_TASKS: AtomicUsize = AtomicUsize::new(0);

    for i in 0..NUM_TASKS {
        ktask::spawn_raw(
            move || {
                println!("sched-fifo: Hello, task {}! ({})", i, current().id_name());
                ktask::yield_now();
                let order = FINISHED_TASKS.fetch_add(1, Ordering::Release);
                assert_eq!(order, i); // FIFO scheduler
            },
            format!("T{i}"),
            0x1000,
        );
    }

    while FINISHED_TASKS.load(Ordering::Acquire) < NUM_TASKS {
        ktask::yield_now();
    }
}

#[test]
fn test_fp_state_switch() {
    let _lock = SERIAL.lock();
    INIT.call_once(ktask::init_scheduler);

    const NUM_TASKS: usize = 5;
    const FLOATS: [f64; NUM_TASKS] = [
        std::f64::consts::PI,
        std::f64::consts::E,
        -std::f64::consts::SQRT_2,
        0.0,
        0.618033988749895,
    ];
    static FINISHED_TASKS: AtomicUsize = AtomicUsize::new(0);

    for (i, float) in FLOATS.iter().enumerate() {
        ktask::spawn(move || {
            let mut value = float + i as f64;
            ktask::yield_now();
            value -= i as f64;

            println!("fp_state_switch: Float {i} = {value}");
            assert!((value - float).abs() < 1e-9);
            FINISHED_TASKS.fetch_add(1, Ordering::Release);
        });
    }
    while FINISHED_TASKS.load(Ordering::Acquire) < NUM_TASKS {
        ktask::yield_now();
    }
}

#[test]
fn test_wait_queue() {
    let _lock = SERIAL.lock();
    INIT.call_once(ktask::init_scheduler);

    const NUM_TASKS: usize = 10;

    static WQ1: WaitQueue = WaitQueue::new();
    static WQ2: WaitQueue = WaitQueue::new();
    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    for _ in 0..NUM_TASKS {
        ktask::spawn(move || {
            COUNTER.fetch_add(1, Ordering::Release);
            println!("wait_queue: task {} started", current().owner_key());
            WQ1.notify_one(true); // WQ1.wait_until()
            WQ2.wait();

            COUNTER.fetch_sub(1, Ordering::Release);
            println!("wait_queue: task {} finished", current().owner_key());
            WQ1.notify_one(true); // WQ1.wait_until()
        });
    }

    println!(
        "task {} is waiting for tasks to start...",
        current().owner_key()
    );
    WQ1.wait_until(|| COUNTER.load(Ordering::Acquire) == NUM_TASKS);
    ktask::yield_now();
    assert_eq!(COUNTER.load(Ordering::Acquire), NUM_TASKS);
    WQ2.notify_all(true); // WQ2.wait()

    println!(
        "task {:?} is waiting for tasks to finish...",
        current().owner_key()
    );
    WQ1.wait_until(|| COUNTER.load(Ordering::Acquire) == 0);
    assert_eq!(COUNTER.load(Ordering::Acquire), 0);
}

#[test]
fn test_task_join() {
    let _lock = SERIAL.lock();
    INIT.call_once(ktask::init_scheduler);

    const NUM_TASKS: usize = 10;
    let mut tasks = Vec::with_capacity(NUM_TASKS);

    for i in 0..NUM_TASKS {
        tasks.push(ktask::spawn_raw(
            move || {
                println!("task_join: task {}! ({})", i, current().id_name());
                ktask::yield_now();
                ktask::exit(i as _);
            },
            format!("T{i}"),
            0x1000,
        ));
    }

    for (i, task) in tasks.into_iter().enumerate() {
        assert_eq!(task.join(), i as _);
    }
}

#[test]
fn test_kirq_sync_wait_provider_blocks_until_completion() {
    let _lock = SERIAL.lock();
    INIT.call_once(ktask::init_scheduler);

    static WAITING: AtomicUsize = AtomicUsize::new(0);
    static FINISHED: AtomicUsize = AtomicUsize::new(0);
    let completion = std::sync::Arc::new(kpoll::Completion::new());
    let waiter_completion = completion.clone();

    WAITING.store(0, Ordering::Release);
    FINISHED.store(0, Ordering::Release);
    ktask::spawn(move || {
        WAITING.store(1, Ordering::Release);
        kirq::IrqSyncWaitIf::wait_for_completion(&waiter_completion)
            .expect("completion wait should register with current task");
        FINISHED.store(1, Ordering::Release);
    });

    while WAITING.load(Ordering::Acquire) == 0 {
        ktask::yield_now();
    }
    ktask::yield_now();
    assert_eq!(
        FINISHED.load(Ordering::Acquire),
        0,
        "waiter must enter Pending before completion"
    );

    completion.complete_all();
    while FINISHED.load(Ordering::Acquire) == 0 {
        ktask::yield_now();
    }
}

#[test]
fn test_prepare_task_requires_activation() {
    let _lock = SERIAL.lock();
    INIT.call_once(ktask::init_scheduler);

    static EXECUTED: AtomicUsize = AtomicUsize::new(0);

    let task = ktask::prepare_task(
        crate::TaskInner::new_kthread(
            || {
                EXECUTED.fetch_add(1, Ordering::Release);
            },
            format!("prepared-task"),
            0x1000,
        )
        .expect("test kernel thread identity allocation should succeed"),
    );

    ktask::yield_now();
    assert_eq!(
        EXECUTED.load(Ordering::Acquire),
        0,
        "prepared task must not run before activate_task"
    );

    ktask::activate_task(&task);
    while EXECUTED.load(Ordering::Acquire) == 0 {
        ktask::yield_now();
    }
}

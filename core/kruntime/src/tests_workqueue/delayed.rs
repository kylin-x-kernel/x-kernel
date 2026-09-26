// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Live delayed-work races; boot providers remain installed throughout the run.

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kcpu_id_map::KCpuMaskExt;
use ktime_types::TimeSpan;
use kworkng::{DefaultQueue, DelayedWork};
use unittest::def_test;

/// Timer delivery races retiming/cancellation; drained storage remains reusable.
///
/// # Panics
/// Panics on API failure, failed task join, wrong callback CPU, overlapping or
/// excess callbacks, duplicate execution after drain/reuse, or retained captures.
#[def_test(serial)]
fn delayed_retime_cancel_race_drains_before_reuse() {
    const ROUNDS: usize = 64;
    let mut cpus = Vec::new();
    kcpu_id_map::for_each_present_logical_cpu(|_, cpu, _| cpus.push(cpu));
    let producer_cpu = cpus[0];
    let target = *cpus.last().unwrap();
    for queue in [
        DefaultQueue::Normal,
        DefaultQueue::BottomHalf,
        DefaultQueue::HighBottomHalf,
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let is_running = Arc::new(AtomicBool::new(false));
        let callback_calls = calls.clone();
        let callback_running = is_running.clone();
        let work = Arc::pin(DelayedWork::new(move || {
            assert_eq!(khal::percpu::this_cpu_id(), target);
            assert!(!callback_running.swap(true, Ordering::AcqRel));
            callback_calls.fetch_add(1, Ordering::Release);
            assert!(callback_running.swap(false, Ordering::AcqRel));
        }));
        let ready = Arc::new(AtomicUsize::new(0));
        let start = Arc::new(AtomicBool::new(false));
        let mut tasks = Vec::new();
        for (is_modifier, cpu) in [(true, producer_cpu), (false, target)] {
            let work = work.clone();
            let ready = ready.clone();
            let start = start.clone();
            let task = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread(
                move || {
                    assert_eq!(khal::percpu::this_cpu_id(), cpu);
                    ready.fetch_add(1, Ordering::Release);
                    while !start.load(Ordering::Acquire) {
                        ktask::yield_now();
                    }
                    for round in 0..ROUNDS {
                        {
                            let _irq = kspin::NoPreemptIrqSave::new();
                            if is_modifier {
                                let delay = match round % 4 {
                                    0 => TimeSpan::ZERO,
                                    1 => TimeSpan::from_nanos(1),
                                    2 => TimeSpan::from_millis(1),
                                    _ => TimeSpan::from_secs(60),
                                };
                                // SAFETY: parent retains the pinned pair/captures
                                // through both producer joins and synchronous
                                // timer/Work drain. No provider teardown occurs.
                                unsafe {
                                    queue.modify_delayed_work(work.as_ref(), delay, Some(target))
                                }
                                .unwrap();
                            } else {
                                work.cancel_pending();
                            }
                        }
                        ktask::yield_now();
                    }
                },
                "delayed-race-producer".into(),
                kbuild_config::TASK_STACK_SIZE,
            ));
            task.set_cpumask(ktask::KCpuMask::one_shot_logical(cpu));
            tasks.push(task);
        }
        for task in &tasks {
            ktask::activate_task(task);
        }
        while ready.load(Ordering::Acquire) != 2 {
            ktask::yield_now();
        }
        start.store(true, Ordering::Release);
        for task in tasks {
            assert_eq!(task.join(), 0);
        }
        work.cancel_sync().unwrap();
        let baseline = calls.load(Ordering::Acquire);
        assert!(baseline <= ROUNDS);
        assert!(!is_running.load(Ordering::Acquire));
        assert!(!work.cancel_sync().unwrap());

        for round in 1..=16 {
            // SAFETY: competing producers have joined; the parent still owns
            // the pinned pair and drains each reuse before eventual release.
            unsafe {
                queue.queue_delayed_work(work.as_ref(), TimeSpan::from_nanos(1), Some(target))
            }
            .unwrap();
            work.flush().unwrap();
            work.cancel_sync().unwrap();
            assert_eq!(calls.load(Ordering::Acquire), baseline + round);
            assert!(!is_running.load(Ordering::Acquire));
        }
        drop(work);
        assert_eq!(Arc::strong_count(&calls), 1);
        assert_eq!(Arc::strong_count(&is_running), 1);
    }
}

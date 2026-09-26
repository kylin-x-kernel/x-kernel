// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Explicit kernel-only release benchmarks and continuous reuse stress.
//! See `README.md` in this directory for configuration and measurement scope.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicUsize, Ordering};

use kcpu_id_map::{KCpuMaskExt, LogicalCpuId};
use ktime_types::TimeSpan;

use crate::{DefaultQueue, DelayedWorkHandle, Work, WorkHandle};

const QUEUES: [DefaultQueue; 3] = [
    DefaultQueue::Normal,
    DefaultQueue::BottomHalf,
    DefaultQueue::HighBottomHalf,
];

fn now_ns() -> u64 {
    khal::time::monotonic_time()
        .span_since_origin()
        .as_nanos_u64_saturating()
}

fn measure(target: LogicalCpuId, round: usize) {
    for (kind, queue) in QUEUES.into_iter().enumerate() {
        let calls = AtomicUsize::new(0);
        let work = Box::pin(Work::with_callback(|| {
            assert_eq!(khal::percpu::this_cpu_id(), target);
            calls.fetch_add(1, Ordering::Relaxed);
        }));
        let begin = now_ns();
        for _ in 0..1000 {
            // SAFETY: sole producer retains this pinned Work and callback
            // captures until every occurrence and final drain complete.
            // The callback only checks CPU identity and updates an atomic;
            // it never sleeps, so it is valid for normal and both BH services.
            unsafe { queue.queue_work(work.as_ref(), Some(target)) }.unwrap();
            work.flush().unwrap();
        }
        let elapsed = now_ns() - begin;
        work.cancel_sync().unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 1000);
        klogger::kprintln!(
            "NG_BENCH round={round} cpu={} kind={kind} ops=1000 total_ns={elapsed} ns_per_op={}",
            target.as_usize(),
            elapsed / 1000
        );
    }
}

fn stress(target: LogicalCpuId, deadline_ns: u64, progress: Arc<AtomicUsize>) {
    let calls = AtomicUsize::new(0);
    let work = Box::pin(Work::with_callback(|| {
        assert_eq!(khal::percpu::this_cpu_id(), target);
        calls.fetch_add(1, Ordering::Relaxed);
    }));
    let delayed_calls = Arc::new(AtomicUsize::new(0));
    let captured = delayed_calls.clone();
    let delayed = DelayedWorkHandle::new(move || {
        captured.fetch_add(1, Ordering::Relaxed);
    })
    .unwrap();
    let mut expected = 0;
    let mut rounds = 0;
    while now_ns() < deadline_ns {
        for queue in QUEUES {
            // SAFETY: the pinned Work/captures outlive all publications and
            // drains; this task is its only producer and services stay live.
            // The callback only checks CPU identity and updates an atomic;
            // it never sleeps, so it is valid for normal and both BH services.
            unsafe { queue.queue_work(work.as_ref(), Some(target)) }.unwrap();
            let revoked = rounds % 2 == 0 && work.cancel_pending();
            work.flush().unwrap();
            expected += usize::from(!revoked);
            assert_eq!(calls.load(Ordering::Relaxed), expected);
        }
        delayed
            .queue_default(TimeSpan::from_secs(60), Some(target))
            .unwrap();
        delayed
            .modify_default(TimeSpan::from_nanos(1), Some(target))
            .unwrap();
        if rounds % 16 == 0 {
            ktask::sleep(TimeSpan::from_millis(1));
        }
        delayed.flush().unwrap();
        delayed.cancel_sync().unwrap();
        rounds += 1;
        assert_eq!(delayed_calls.load(Ordering::Relaxed), rounds);
        if rounds % 64 == 0 {
            let captures = Arc::new(AtomicUsize::new(0));
            struct Capture(Arc<AtomicUsize>);
            impl Drop for Capture {
                fn drop(&mut self) {
                    self.0.fetch_add(1, Ordering::Release);
                }
            }
            let capture = Capture(captures.clone());
            let owned = WorkHandle::new(move || {
                core::hint::black_box(&capture);
            })
            .unwrap();
            {
                let _irq = kspin::NoPreemptIrqSave::new();
                owned.queue_default(Some(target)).unwrap();
                drop(owned);
            }
            while captures.load(Ordering::Acquire) == 0 {
                ktask::yield_now();
            }
            assert_eq!(captures.load(Ordering::Acquire), 1);
        }
        // Progress counters are observational, not lifetime synchronization.
        progress.fetch_add(1, Ordering::Relaxed);
    }
    work.cancel_sync().unwrap();
    delayed.shutdown().unwrap();
    klogger::kprintln!(
        "NG_SOAK_WORKER target_cpu={} rounds={rounds} callbacks={expected}",
        target.as_usize()
    );
}

/// Runs warmup, five measured rounds, concurrent soak, and five final rounds.
/// Call once in sleepable boot context after all NG providers are installed.
/// Each CPU producer targets the next CPU; one-CPU boots target themselves.
///
/// # Panics
/// Panics on admission/drain errors, incorrect callback counts or placement,
/// failed task joins, a duration above one day, or fatal allocation failure.
/// An external timeout must diagnose hangs; elapsed times are measurements,
/// not pass thresholds.
pub fn run(seconds: usize) {
    assert!(seconds <= 86400);
    let mut cpus = Vec::new();
    kcpu_id_map::for_each_present_logical_cpu(|_, cpu, _| cpus.push(cpu));
    assert!(!cpus.is_empty());
    klogger::kprintln!(
        "NG_RUN cpus={} soak_seconds={seconds} coverage=false",
        cpus.len()
    );
    for round in 0..=5 {
        for &cpu in &cpus {
            measure(cpu, round);
        }
    }
    let progress = Arc::new(AtomicUsize::new(0));
    let begin = now_ns();
    let deadline = begin + seconds as u64 * 1_000_000_000;
    let mut tasks = Vec::new();
    for (index, &cpu) in cpus.iter().enumerate() {
        let target = cpus[(index + 1) % cpus.len()];
        let progress = progress.clone();
        let task = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread(
            move || stress(target, deadline, progress),
            "ng-soak".into(),
            kbuild_config::TASK_STACK_SIZE,
        ));
        task.set_cpumask(ktask::KCpuMask::one_shot_logical(cpu));
        ktask::activate_task(&task);
        tasks.push(task);
    }
    let mut last_report = begin;
    while now_ns() < deadline {
        ktask::sleep(TimeSpan::from_secs(1));
        if now_ns() - last_report >= 60_000_000_000 {
            klogger::kprintln!(
                "NG_SOAK_PROGRESS elapsed_s={} cycles={}",
                (now_ns() - begin) / 1_000_000_000,
                progress.load(Ordering::Relaxed)
            );
            last_report = now_ns();
        }
    }
    for task in tasks {
        assert_eq!(task.join(), 0);
    }
    klogger::kprintln!(
        "NG_SOAK_PASS elapsed_ns={} cycles={}",
        now_ns() - begin,
        progress.load(Ordering::Relaxed)
    );
    for round in 6..=10 {
        for &cpu in &cpus {
            measure(cpu, round);
        }
    }
    klogger::kprintln!("NG_RUN_PASS");
}

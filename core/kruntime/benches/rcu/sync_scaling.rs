// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Concurrent synchronous wait costs with optional finite reader pressure.

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use kcpu_id_map::{KCpuMaskExt, LogicalCpuId};

const SAMPLES: usize = 128;

#[derive(Default)]
struct StartGate {
    ready: AtomicUsize,
    is_open: AtomicBool,
}

#[derive(Default)]
struct ReaderProgress {
    completed: AtomicUsize,
    min_body_ns: AtomicU64,
    max_body_ns: AtomicU64,
}

fn start_reader(
    cpu: LogicalCpuId,
    gate: Arc<StartGate>,
    should_stop: Arc<AtomicBool>,
    hold_ns: u64,
    is_rcu_reader: bool,
) -> (ktask::KtaskRef, Arc<ReaderProgress>) {
    let progress = Arc::new(ReaderProgress::default());
    let output = progress.clone();
    let task = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread(
        move || {
            gate.ready.fetch_add(1, Ordering::Release);
            while !gate.is_open.load(Ordering::Acquire) {
                ktask::yield_now();
            }
            let mut minimum = u64::MAX;
            let mut maximum = 0;
            while !should_stop.load(Ordering::Relaxed) {
                assert_eq!(khal::percpu::this_cpu_id(), cpu);
                let busy_read_fn = || {
                    let start = khal::time::monotonic_time_nanos();
                    loop {
                        let now = khal::time::monotonic_time_nanos();
                        assert!(now >= start);
                        if now - start >= hold_ns {
                            break now - start;
                        }
                        // No lock, IRQ masking, blocking or cooperative yield
                        // in the scope. Involuntary preemption remains allowed.
                        core::hint::spin_loop();
                    }
                };
                let duration = if is_rcu_reader {
                    krcu::with_read(|_| busy_read_fn())
                } else {
                    busy_read_fn()
                };
                minimum = minimum.min(duration);
                maximum = maximum.max(duration);
                output.completed.fetch_add(1, Ordering::Relaxed);
                ktask::yield_now();
            }
            output.min_body_ns.store(minimum, Ordering::Relaxed);
            output.max_body_ns.store(maximum, Ordering::Relaxed);
        },
        "rcu-sync-reader".into(),
        kbuild_config::TASK_STACK_SIZE,
    ));
    task.set_cpumask(ktask::KCpuMask::one_shot_logical(cpu));
    ktask::activate_task(&task);
    (task, progress)
}

fn run_load(cpus: &[LogicalCpuId], hold_ns: u64, is_rcu_reader: bool) {
    let read_scopes = usize::from(is_rcu_reader);
    for workers in [1, 2, 4] {
        if workers > cpus.len() {
            continue;
        }
        for round in 0..=super::ROUNDS {
            let gate = Arc::new(StartGate::default());
            let should_stop = Arc::new(AtomicBool::new(false));
            let readers: Vec<_> = if hold_ns == 0 {
                Vec::new()
            } else {
                cpus.iter()
                    .map(|&cpu| {
                        start_reader(
                            cpu,
                            gate.clone(),
                            should_stop.clone(),
                            hold_ns,
                            is_rcu_reader,
                        )
                    })
                    .collect()
            };
            let mut tasks = Vec::with_capacity(workers);
            let mut results = Vec::with_capacity(workers);
            for &cpu in &cpus[..workers] {
                let output = Arc::new(kmutex::Mutex::new(Vec::new()));
                results.push(output.clone());
                let gate = gate.clone();
                let task = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread(
                    move || {
                        let mut samples = Vec::with_capacity(SAMPLES);
                        gate.ready.fetch_add(1, Ordering::Release);
                        // Yield outside read scopes so the gate works on UP too.
                        while !gate.is_open.load(Ordering::Acquire) {
                            ktask::yield_now();
                        }
                        for _ in 0..SAMPLES {
                            assert_eq!(khal::percpu::this_cpu_id(), cpu);
                            let start = khal::time::monotonic_time_nanos();
                            // SAFETY: watched sleepable worker, no read scope or
                            // held locks. No object is retired by this benchmark.
                            let result = unsafe { krcu::synchronize_rcu() };
                            let end = khal::time::monotonic_time_nanos();
                            assert!(result.is_ok());
                            assert!(end >= start);
                            samples.push(end - start);
                        }
                        *output.lock() = samples;
                    },
                    "rcu-sync-scaling".into(),
                    kbuild_config::TASK_STACK_SIZE,
                ));
                task.set_cpumask(ktask::KCpuMask::one_shot_logical(cpu));
                ktask::activate_task(&task);
                tasks.push(task);
            }
            while gate.ready.load(Ordering::Acquire) != workers + readers.len() {
                ktask::yield_now();
            }
            // Both endpoints use the coordinator's clock. Includes worker start
            // and join scheduling, not allocation or per-sample output logging.
            let start = khal::time::monotonic_time_nanos();
            gate.is_open.store(true, Ordering::Release);
            for task in &tasks {
                assert_eq!(task.join(), 0);
            }
            let end = khal::time::monotonic_time_nanos();
            assert!(end >= start);
            let elapsed_ns = end - start;
            // Snapshot before stopping readers: final joins must not turn an
            // unexercised load into a passing observation. Cleanup is untimed.
            let reader_counts: Vec<_> = readers
                .iter()
                .map(|(_, progress)| progress.completed.load(Ordering::Relaxed))
                .collect();
            should_stop.store(true, Ordering::Relaxed);
            for (index, (task, progress)) in readers.iter().enumerate() {
                assert_eq!(task.join(), 0);
                assert!(reader_counts[index] > 0, "reader load did not execute");
                let minimum = progress.min_body_ns.load(Ordering::Relaxed);
                let maximum = progress.max_body_ns.load(Ordering::Relaxed);
                assert!(minimum >= hold_ns && maximum >= minimum);
                klogger::kprintln!(
                    "RCU_SYNC_READER workers={workers} round={round} hold_ns={hold_ns} \
                     read_scopes={read_scopes} cpu={} completed_in_window={} total={} \
                     min_body_ns={minimum} max_body_ns={maximum}",
                    cpus[index].as_usize(),
                    reader_counts[index],
                    progress.completed.load(Ordering::Relaxed)
                );
            }
            report(cpus, &results, round, hold_ns, elapsed_ns, read_scopes);
        }
    }
}

fn report(
    cpus: &[LogicalCpuId],
    results: &[Arc<kmutex::Mutex<Vec<u64>>>],
    round: usize,
    hold_ns: u64,
    elapsed_ns: u64,
    read_scopes: usize,
) {
    let workers = results.len();
    let mut combined = Vec::with_capacity(workers * SAMPLES);
    for (worker, output) in results.iter().enumerate() {
        let samples = core::mem::take(&mut *output.lock());
        assert_eq!(samples.len(), SAMPLES);
        for (index, elapsed_ns) in samples.iter().enumerate() {
            klogger::kprintln!(
                "RCU_SYNC_SCALING_SAMPLE workers={workers} round={round} hold_ns={hold_ns} \
                 read_scopes={read_scopes} worker={worker} cpu={} index={index} \
                 elapsed_ns={elapsed_ns}",
                cpus[worker].as_usize()
            );
        }
        combined.extend(samples);
    }
    combined.sort_unstable();
    let percentile_fn = |percent: usize| combined[(combined.len() * percent).div_ceil(100) - 1];
    klogger::kprintln!(
        "RCU_SYNC_SCALING workers={workers} round={round} hold_ns={hold_ns} \
         read_scopes={read_scopes} completed={} elapsed_ns={elapsed_ns} p50_ns={} p95_ns={} \
         p99_ns={} max_ns={}",
        combined.len(),
        percentile_fn(50),
        percentile_fn(95),
        percentile_fn(99),
        combined[combined.len() - 1]
    );
}

/// Measures full public calls, including allocation, GP and scheduling costs.
pub(super) fn run(cpus: &[LogicalCpuId]) {
    run_load(cpus, 0, false);
    for hold_ns in [10_000, 1_000_000] {
        run_load(cpus, hold_ns, false);
        run_load(cpus, hold_ns, true);
    }
    klogger::kprintln!("RCU_SYNC_SCALING_PASS");
}

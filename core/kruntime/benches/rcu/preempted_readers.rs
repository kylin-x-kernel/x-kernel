// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Same-CPU competitors witness actual switches inside non-yielding bodies.

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kcpu_id_map::{KCpuMaskExt, LogicalCpuId};

const BODIES: usize = 16;
const BODY_NS: u64 = 10_000_000;

#[derive(Default)]
struct Progress {
    is_ready: AtomicBool,
    is_done: AtomicBool,
    active_body: AtomicUsize,
    witnessed_bodies: AtomicUsize,
}

fn run_round(cpus: &[LogicalCpuId], round: usize, use_rcu: bool) {
    let gate = Arc::new(AtomicBool::new(false));
    let mut readers = Vec::new();
    let mut competitors = Vec::new();
    for &cpu in cpus {
        let progress = Arc::new(Progress::default());
        let output = Arc::new(ksync::Mutex::new((0u64, 0u64)));
        let observed = progress.clone();
        let competitor = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread(
            move || {
                let mut last_body = 0;
                while !observed.is_done.load(Ordering::Acquire) {
                    assert_eq!(khal::percpu::this_cpu_id(), cpu);
                    let body = observed.active_body.load(Ordering::Acquire);
                    if body != 0 && body != last_body {
                        last_body = body;
                        observed.witnessed_bodies.fetch_add(1, Ordering::Relaxed);
                    }
                    ktask::yield_now();
                }
            },
            "rcu-reader-competitor".into(),
            kbuild_config::TASK_STACK_SIZE,
        ));
        competitor.set_cpumask(ktask::KCpuMask::one_shot_logical(cpu));
        ktask::activate_task(&competitor);
        competitors.push(competitor);

        let state = progress.clone();
        let result = output.clone();
        let start_gate = gate.clone();
        let reader = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread(
            move || {
                state.is_ready.store(true, Ordering::Release);
                while !start_gate.load(Ordering::Acquire) {
                    ktask::yield_now();
                }
                let begin = khal::time::monotonic_time_nanos();
                let mut operations = 0u64;
                for body in 1..=BODIES {
                    let body_fn = || {
                        assert_eq!(khal::percpu::this_cpu_id(), cpu);
                        state.active_body.store(body, Ordering::Release);
                        let start = khal::time::monotonic_time_nanos();
                        let mut count = 0u64;
                        while khal::time::monotonic_time_nanos() - start < BODY_NS {
                            // Fixed useful work between clock reads; never yield in the body.
                            for value in 0..64u64 {
                                core::hint::black_box(value.wrapping_mul(17));
                            }
                            count += 64;
                        }
                        state.active_body.store(0, Ordering::Release);
                        count
                    };
                    operations += if use_rcu {
                        krcu::with_read(|_| body_fn())
                    } else {
                        body_fn()
                    };
                    ktask::yield_now();
                }
                let elapsed_ns = khal::time::monotonic_time_nanos() - begin;
                state.is_done.store(true, Ordering::Release);
                *result.lock() = (operations, elapsed_ns);
            },
            "rcu-preempted-reader".into(),
            kbuild_config::TASK_STACK_SIZE,
        ));
        reader.set_cpumask(ktask::KCpuMask::one_shot_logical(cpu));
        ktask::activate_task(&reader);
        readers.push((reader, progress, output));
    }
    while readers
        .iter()
        .any(|(_, state, _)| !state.is_ready.load(Ordering::Acquire))
    {
        ktask::yield_now();
    }
    let begin = khal::time::monotonic_time_nanos();
    gate.store(true, Ordering::Release);
    for (reader, ..) in &readers {
        assert_eq!(reader.join(), 0);
    }
    let elapsed_ns = khal::time::monotonic_time_nanos() - begin;
    for competitor in competitors {
        assert_eq!(competitor.join(), 0);
    }
    let mut total_operations = 0u64;
    for (index, (_, state, output)) in readers.iter().enumerate() {
        let witnessed = state.witnessed_bodies.load(Ordering::Relaxed);
        assert!(
            witnessed > 0 && witnessed <= BODIES,
            "no actual in-body switch witnessed"
        );
        let (operations, reader_ns) = *output.lock();
        assert!(operations > 0 && reader_ns > 0);
        total_operations += operations;
        klogger::kprintln!(
            "RCU_PREEMPT_READER workers={} round={round} read_scopes={} cpu={} bodies={BODIES} \
             witnessed_bodies={witnessed} operations={operations} elapsed_ns={reader_ns}",
            cpus.len(),
            usize::from(use_rcu),
            cpus[index].as_usize()
        );
    }
    klogger::kprintln!(
        "RCU_PREEMPT_BATCH workers={} round={round} read_scopes={} operations={total_operations} \
         elapsed_ns={elapsed_ns}",
        cpus.len(),
        usize::from(use_rcu)
    );
}

pub(super) fn run(cpus: &[LogicalCpuId]) {
    // ktask directly enables kspin/preempt; actual in-body switches are checked below.
    for workers in [1, 2, 4] {
        if workers > cpus.len() {
            continue;
        }
        for round in 0..=super::ROUNDS {
            for use_rcu in if round % 2 == 0 {
                [false, true]
            } else {
                [true, false]
            } {
                run_round(&cpus[..workers], round, use_rcu);
            }
        }
    }
    klogger::kprintln!("RCU_PREEMPT_PASS");
}

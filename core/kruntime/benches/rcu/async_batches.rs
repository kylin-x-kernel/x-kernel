// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Bounded bursts; submission, callback entry and drained completion are distinct.

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};

use kcpu_id_map::{KCpuMaskExt, LogicalCpuId};

use super::{Observation, prepare};

struct Submission {
    begin_ns: u64,
    end_ns: u64,
    observation: Arc<Observation>,
}

fn run_round(cpus: &[LogicalCpuId], batch: usize, round: usize) {
    let can_start = Arc::new(AtomicBool::new(false));
    let mut workers = Vec::with_capacity(cpus.len());
    let mut outputs = Vec::with_capacity(cpus.len());
    // Prepare all storage before opening the gate, so allocator pressure is
    // excluded from submit timing and total record storage is strictly bounded.
    for &cpu in cpus {
        let records: Vec<_> = (0..batch)
            .map(|_| {
                let observation = Arc::new(Observation::default());
                (prepare(cpu, &observation), observation)
            })
            .collect();
        let mut samples = Vec::with_capacity(batch);
        let output = Arc::new(ksync::Mutex::new(Vec::new()));
        outputs.push(output.clone());
        let can_start = can_start.clone();
        let worker = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread(
            move || {
                while !can_start.load(Ordering::Acquire) {
                    ktask::yield_now();
                }
                assert_eq!(khal::percpu::this_cpu_id(), cpu);
                for (prepared, observation) in records {
                    let begin_ns = khal::time::monotonic_time_nanos();
                    // SAFETY: records were never published, own stable storage
                    // and use the existing nonblocking callback. This pinned
                    // watched task holds no locks; code/service remain live.
                    let accepted = unsafe { prepared.submit() };
                    let end_ns = khal::time::monotonic_time_nanos();
                    assert!(accepted.is_ok(), "RCU burst submission rejected");
                    samples.push(Submission {
                        begin_ns,
                        end_ns,
                        observation,
                    });
                }
                *output.lock() = samples;
            },
            "rcu-async-batch".into(),
            kbuild_config::TASK_STACK_SIZE,
        ));
        worker.set_cpumask(ktask::KCpuMask::one_shot_logical(cpu));
        workers.push(worker);
    }
    for worker in &workers {
        ktask::activate_task(worker);
    }
    let begin_ns = khal::time::monotonic_time_nanos();
    can_start.store(true, Ordering::Release);
    for worker in &workers {
        assert_eq!(worker.join(), 0);
    }
    let samples: Vec<Vec<Submission>> = outputs
        .iter()
        .map(|output| core::mem::take(&mut *output.lock()))
        .collect();
    let not_entered = samples
        .iter()
        .flatten()
        .filter(|sample| sample.observation.entry_ns.load(Ordering::Acquire) == 0)
        .count();
    let drain_begin_ns = khal::time::monotonic_time_nanos();
    // SAFETY: all producers joined, no repost/downstream work, no held locks
    // or read scopes, watched sleepable task. Observations and code remain live.
    let drained = unsafe { krcu::drain_callbacks() };
    let completed_ns = khal::time::monotonic_time_nanos();
    assert!(drained.is_ok());
    assert!(begin_ns <= drain_begin_ns && drain_begin_ns <= completed_ns);
    for (cpu, samples) in cpus.iter().zip(&samples) {
        assert_eq!(samples.len(), batch);
        for (index, sample) in samples.iter().enumerate() {
            let entry_ns = sample.observation.entry_ns.load(Ordering::Acquire);
            assert!(!sample.observation.has_wrong_context.load(Ordering::Relaxed));
            assert!(sample.begin_ns <= sample.end_ns && sample.end_ns <= completed_ns);
            assert!(sample.begin_ns <= entry_ns && entry_ns <= completed_ns);
            klogger::kprintln!(
                "RCU_ASYNC_SAMPLE workers={} batch={batch} round={round} cpu={} index={index} \
                 submit_ns={} callback_entry_ns={} drained_upper_ns={}",
                cpus.len(),
                cpu.as_usize(),
                sample.end_ns - sample.begin_ns,
                entry_ns - sample.begin_ns,
                completed_ns - sample.begin_ns
            );
        }
    }
    klogger::kprintln!(
        "RCU_ASYNC_BATCH workers={} batch={batch} round={round} accepted={} \
         not_entered_before_drain={not_entered} drain_ns={} elapsed_ns={}",
        cpus.len(),
        cpus.len() * batch,
        completed_ns - drain_begin_ns,
        completed_ns - begin_ns
    );
}

pub(super) fn run(cpus: &[LogicalCpuId]) {
    for workers in [1, 2, 4] {
        if workers > cpus.len() {
            continue;
        }
        for batch in [32, 256] {
            for round in 0..=3 {
                run_round(&cpus[..workers], batch, round);
            }
        }
    }
    klogger::kprintln!("RCU_ASYNC_PASS");
}

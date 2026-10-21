// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Prepared submission, callback completion, synchronous wait and read costs.

#[path = "async_batches.rs"]
mod async_batches;
#[path = "backlog.rs"]
mod backlog;
#[path = "preempted_readers.rs"]
mod preempted_readers;
#[path = "read_cost.rs"]
mod read_cost;
#[path = "sync_scaling.rs"]
mod sync_scaling;

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::{
    ptr::NonNull,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

use kcpu_id_map::{KCpuMaskExt, LogicalCpuId};

use crate::{CallbackNode, PreparedCallback};

const SAMPLES: usize = 32;
const ROUNDS: usize = 3;

#[derive(Default)]
struct Observation {
    entry_ns: AtomicU64,
    has_wrong_context: AtomicBool,
}

#[repr(C)]
struct Record {
    node: CallbackNode,
    observation: Arc<Observation>,
    cpu: LogicalCpuId,
}

const _: () = assert!(core::mem::offset_of!(Record, node) == 0);

/// # Safety
/// Consume the unique raw Box projection created by prepare, once after queue
/// detachment. No other owner may access the record after accepted submission.
unsafe fn invoke(node: *mut CallbackNode) {
    let entry_ns = khal::time::monotonic_time_nanos();
    // SAFETY: repr(C) and the offset assertion preserve the original Box base.
    let record = unsafe { Box::from_raw(node.cast::<Record>()) };
    let is_valid = khal::percpu::this_cpu_id() == record.cpu
        && kirq::context::interrupt_context_level()
            == kirq::context::InterruptContextLevel::Softirq;
    record
        .observation
        .has_wrong_context
        .store(!is_valid, Ordering::Relaxed);
    record
        .observation
        .entry_ns
        .store(entry_ns, Ordering::Release);
}

/// # Safety
/// Consume the same raw Box projection as invoke, but only while its sole
/// submission handle owns the unqueued record. It was never published to readers.
unsafe fn cancel(node: *mut CallbackNode) {
    // SAFETY: cancellation retains the unique unsubmitted allocation owner.
    drop(unsafe { Box::from_raw(node.cast::<Record>()) });
}

fn prepare(cpu: LogicalCpuId, observation: &Arc<Observation>) -> PreparedCallback {
    let record = Box::into_raw(Box::new(Record {
        node: CallbackNode::new(invoke),
        observation: observation.clone(),
        cpu,
    }));
    // SAFETY: raw field projection retains the enclosing allocation's provenance.
    let node = unsafe { &raw mut (*record).node };
    // SAFETY: the initialized field of this live Box cannot be null.
    let node = unsafe { NonNull::new_unchecked(node) };
    // SAFETY: sole stable submission owner, never published to readers. Both
    // cancellation and invocation release the Box without blocking. Observation
    // storage and permanent callback code outlive either path on any CPU.
    unsafe { PreparedCallback::new(node, cancel) }
}

struct Sample {
    submit_ns: u64,
    callback_entry_ns: u64,
    completion_observed_ns: u64,
}

#[derive(Default)]
struct RoundSamples {
    callbacks: Vec<Sample>,
    sync_wait_ns: Vec<u64>,
    reads: Vec<[u64; 3]>,
}

fn measure_sync_wait() -> Vec<u64> {
    let mut samples = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let begin = khal::time::monotonic_time_nanos();
        // SAFETY: this watched, sleepable boot worker holds no reader scope or
        // lock needed by readers/service. No protected object is being retired;
        // the permanent initialized service supplies the actual GP protocol.
        let result = unsafe { crate::synchronize_rcu() };
        let end = khal::time::monotonic_time_nanos();
        assert!(result.is_ok());
        assert!(end >= begin);
        samples.push(end - begin);
    }
    samples
}

fn measure(cpu: LogicalCpuId) -> Vec<Sample> {
    assert_eq!(khal::percpu::this_cpu_id(), cpu);
    let mut samples = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let observation = Arc::new(Observation::default());
        let prepared = prepare(cpu, &observation);
        let begin = khal::time::monotonic_time_nanos();
        // SAFETY: unpublished record, nonblocking callback, permanent boot
        // service/code, no RCU or scheduler locks held. Preparation is untimed.
        let accepted = unsafe { prepared.submit() };
        let submitted = khal::time::monotonic_time_nanos();
        if let Err((error, returned)) = accepted {
            drop(returned);
            klogger::kprintln!("RCU_LATENCY_REJECT error={error:?}");
            // The coordinator requires all samples; a partial round fails
            // before it can be reported as a valid latency distribution.
            return samples;
        }
        // SAFETY: this sequential producer has stopped submitting, no reposts
        // or downstream work. Sleepable watched task holds no callback resource
        // locks; the observation remains owned until callback return is drained.
        let drained = unsafe { crate::drain_callbacks() };
        let completed = khal::time::monotonic_time_nanos();
        assert!(drained.is_ok());
        let entry = observation.entry_ns.load(Ordering::Acquire);
        assert!(!observation.has_wrong_context.load(Ordering::Relaxed));
        assert!(begin <= submitted && submitted <= completed);
        assert!(begin <= entry && entry <= completed);
        samples.push(Sample {
            submit_ns: submitted - begin,
            callback_entry_ns: entry - begin,
            completion_observed_ns: completed - begin,
        });
    }
    samples
}

fn report(cpu: LogicalCpuId, round: usize, metric: &str, mut values: Vec<u64>) {
    values.sort_unstable();
    let percentile_fn = |percent: usize| values[(values.len() * percent).div_ceil(100) - 1];
    klogger::kprintln!(
        "RCU_LATENCY cpu={} round={round} metric={metric} n={} p50_ns={} p95_ns={} p99_ns={} \
         max_ns={}",
        cpu.as_usize(),
        values.len(),
        percentile_fn(50),
        percentile_fn(95),
        percentile_fn(99),
        values[values.len() - 1]
    );
}

/// Runs one warmup and three measured rounds on each present CPU serially.
/// Call once after runtime initialization, in sleepable task context.
///
/// # Panics
/// Panics on rejected operations, failed joins, wrong callback context/CPU or
/// inconsistent clock observations. There is no latency pass threshold.
pub fn run() {
    let mut cpus = Vec::new();
    kcpu_id_map::for_each_present_logical_cpu(|_, cpu, _| cpus.push(cpu));
    assert!(!cpus.is_empty());
    klogger::kprintln!(
        "RCU_LATENCY_BEGIN cpus={} samples={SAMPLES} rounds={ROUNDS} coverage=false",
        cpus.len()
    );
    for &cpu in &cpus {
        for round in 0..=ROUNDS {
            let result = Arc::new(ksync::Mutex::new(RoundSamples::default()));
            let output = result.clone();
            let task = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread(
                move || {
                    let callbacks = measure(cpu);
                    let sync_wait_ns = measure_sync_wait();
                    let reads = read_cost::measure();
                    *output.lock() = RoundSamples {
                        callbacks,
                        sync_wait_ns,
                        reads,
                    };
                },
                "rcu-latency".into(),
                kbuild_config::TASK_STACK_SIZE,
            ));
            task.set_cpumask(ktask::KCpuMask::one_shot_logical(cpu));
            ktask::activate_task(&task);
            assert_eq!(task.join(), 0);
            let RoundSamples {
                callbacks: samples,
                sync_wait_ns,
                reads,
            } = core::mem::take(&mut *result.lock());
            read_cost::report(cpu, round, &reads);
            assert_eq!(samples.len(), SAMPLES);
            assert_eq!(sync_wait_ns.len(), SAMPLES);
            for (index, elapsed_ns) in sync_wait_ns.iter().enumerate() {
                klogger::kprintln!(
                    "RCU_SYNC_SAMPLE cpu={} round={round} index={index} elapsed_ns={elapsed_ns}",
                    cpu.as_usize()
                );
            }
            report(cpu, round, "sync_wait", sync_wait_ns);
            for (index, sample) in samples.iter().enumerate() {
                klogger::kprintln!(
                    "RCU_LATENCY_SAMPLE cpu={} round={round} index={index} submit_ns={} \
                     callback_entry_ns={} completion_observed_ns={}",
                    cpu.as_usize(),
                    sample.submit_ns,
                    sample.callback_entry_ns,
                    sample.completion_observed_ns
                );
            }
            report(
                cpu,
                round,
                "submit",
                samples.iter().map(|s| s.submit_ns).collect(),
            );
            report(
                cpu,
                round,
                "callback_entry",
                samples.iter().map(|s| s.callback_entry_ns).collect(),
            );
            report(
                cpu,
                round,
                "completion_observed",
                samples.iter().map(|s| s.completion_observed_ns).collect(),
            );
        }
    }
    async_batches::run(&cpus);
    backlog::run(&cpus);
    sync_scaling::run(&cpus);
    preempted_readers::run(&cpus);
    klogger::kprintln!("RCU_LATENCY_PASS");
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! A finite old reader creates a known backlog; per-record release timestamps
//! reconstruct its decay without polling the production queue.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::{
    ptr::NonNull,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

use kcpu_id_map::{KCpuMaskExt, LogicalCpuId};
use krcu::{CallbackNode, PreparedCallback};

const TIMEOUT_NS: u64 = 5_000_000_000;

#[derive(Default)]
struct ReaderGate {
    is_ready: AtomicBool,
    can_exit: AtomicBool,
    has_timed_out: AtomicBool,
}

#[repr(C)]
struct Record {
    node: CallbackNode,
    released_ns: Arc<AtomicU64>,
}

const _: () = assert!(core::mem::offset_of!(Record, node) == 0);

/// # Safety
/// Consume exactly once the zero-offset node projection of the Box transferred
/// by prepare, after detachment. No other owner may access that allocation.
unsafe fn invoke(node: *mut CallbackNode) {
    // SAFETY: repr(C) and the offset assertion recover the submitted Box base.
    let record = unsafe { Box::from_raw(node.cast::<Record>()) };
    let observation = record.released_ns.clone();
    assert_eq!(
        kirq::context::interrupt_context_level(),
        kirq::context::InterruptContextLevel::Softirq
    );
    drop(record);
    // This is after record destruction, but before this callback returns.
    // Retain the observation independently; never inspect the freed node.
    observation.store(khal::time::monotonic_time_nanos(), Ordering::Release);
}

/// # Safety
/// Consume the same Box projection as invoke, solely while unsubmitted.
unsafe fn cancel(node: *mut CallbackNode) {
    // SAFETY: cancellation owns this unqueued allocation exclusively.
    drop(unsafe { Box::from_raw(node.cast::<Record>()) });
}

fn prepare(observation: &Arc<AtomicU64>) -> PreparedCallback {
    let record = Box::into_raw(Box::new(Record {
        node: CallbackNode::new(invoke),
        released_ns: observation.clone(),
    }));
    // SAFETY: projection preserves provenance of this initialized live Box.
    let node = unsafe { &raw mut (*record).node };
    // SAFETY: the initialized field of a live allocation cannot be null.
    let node = unsafe { NonNull::new_unchecked(node) };
    // SAFETY: unique stable storage never published to readers; cancellation
    // and callback release it once without blocking. Permanent code and the
    // independently retained observation outlive either transfer path.
    unsafe { PreparedCallback::new(node, cancel) }
}

fn run_round(reader_cpu: LogicalCpuId, count: usize, round: usize) {
    let observations: Vec<_> = (0..count).map(|_| Arc::new(AtomicU64::new(0))).collect();
    let records: Vec<_> = observations.iter().map(prepare).collect();
    let gate = Arc::new(ReaderGate::default());
    let reader_gate = gate.clone();
    let reader = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread(
        move || {
            krcu::with_read(|_| {
                let start_ns = khal::time::monotonic_time_nanos();
                // Acquire on the producer side orders every submission after
                // entry to this old read scope, before any covering GP begins.
                reader_gate.is_ready.store(true, Ordering::Release);
                while !reader_gate.can_exit.load(Ordering::Acquire) {
                    if khal::time::monotonic_time_nanos() - start_ns >= TIMEOUT_NS {
                        reader_gate.has_timed_out.store(true, Ordering::Release);
                        break;
                    }
                    core::hint::spin_loop();
                }
            });
        },
        "rcu-backlog-reader".into(),
        kbuild_config::TASK_STACK_SIZE,
    ));
    reader.set_cpumask(ktask::KCpuMask::one_shot_logical(reader_cpu));
    ktask::activate_task(&reader);
    let start_ns = khal::time::monotonic_time_nanos();
    while !gate.is_ready.load(Ordering::Acquire) {
        assert!(khal::time::monotonic_time_nanos() - start_ns < TIMEOUT_NS);
        ktask::yield_now();
    }
    for record in records {
        // SAFETY: unpublished stable records, permanent service/code, watched
        // task without held locks or reader scope; callback never blocks.
        let accepted = unsafe { record.submit() };
        assert!(accepted.is_ok());
    }
    assert!(!gate.has_timed_out.load(Ordering::Acquire));
    assert!(
        observations
            .iter()
            .all(|stamp| stamp.load(Ordering::Acquire) == 0)
    );
    let release_ns = khal::time::monotonic_time_nanos();
    gate.can_exit.store(true, Ordering::Release);
    // SAFETY: no producers remain, no reposts, no held locks/read scope in this
    // watched sleepable task; finite reader is released and all code stays live.
    let drained = unsafe { krcu::drain_callbacks() };
    let drained_ns = khal::time::monotonic_time_nanos();
    assert!(drained.is_ok());
    assert_eq!(reader.join(), 0);
    assert!(!gate.has_timed_out.load(Ordering::Acquire));
    let mut releases: Vec<_> = observations
        .iter()
        .map(|stamp| stamp.load(Ordering::Acquire))
        .collect();
    assert!(
        releases
            .iter()
            .all(|&stamp| release_ns <= stamp && stamp <= drained_ns)
    );
    releases.sort_unstable();
    for (index, stamp) in releases.iter().enumerate() {
        klogger::kprintln!(
            "RCU_BACKLOG_RELEASE count={count} round={round} index={index} elapsed_ns={} \
             remaining={}",
            stamp - release_ns,
            count - index - 1
        );
    }
    klogger::kprintln!(
        "RCU_BACKLOG count={count} round={round} peak_records={count} peak_record_bytes={} \
         released={} remaining=0 release_to_drain_ns={}",
        count * core::mem::size_of::<Record>(),
        releases.len(),
        drained_ns - release_ns
    );
}

pub(super) fn run(cpus: &[LogicalCpuId]) {
    if cpus.len() < 2 {
        klogger::kprintln!("RCU_BACKLOG_SKIP reason=requires_multiple_cpus");
        return;
    }
    for count in [256, 1024] {
        for round in 0..=3 {
            run_round(cpus[cpus.len() - 1], count, round);
        }
    }
    klogger::kprintln!("RCU_BACKLOG_PASS");
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Explicit release ownership stress, not a latency benchmark or memory model.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::Rcu;

#[path = "retirement.rs"]
mod retirement;

#[path = "callbacks.rs"]
mod callbacks;

const WRITERS: usize = 2;
const UPDATES_PER_WRITER: usize = 32;
const OBJECTS: usize = 1 + WRITERS * UPDATES_PER_WRITER;

struct Ledger {
    drops: [AtomicUsize; OBJECTS],
    returned: [AtomicUsize; OBJECTS],
    reads: Vec<AtomicUsize>,
    updated_reads: AtomicUsize,
    errors: AtomicUsize,
    should_stop: AtomicBool,
}

struct Payload {
    id: usize,
    words: [usize; 8],
    ledger: Arc<Ledger>,
}

impl Payload {
    fn new(id: usize, ledger: &Arc<Ledger>) -> Box<Self> {
        Box::new(Self {
            id,
            words: core::array::from_fn(|index| id.wrapping_mul(37) ^ index),
            ledger: ledger.clone(),
        })
    }

    fn is_intact(&self) -> bool {
        self.id < OBJECTS
            && self
                .words
                .iter()
                .enumerate()
                .all(|(index, word)| *word == (self.id.wrapping_mul(37) ^ index))
            && self.ledger.drops[self.id].load(Ordering::Relaxed) == 0
    }
}

impl Drop for Payload {
    fn drop(&mut self) {
        self.ledger.drops[self.id].fetch_add(1, Ordering::Relaxed);
    }
}

fn run_reader(value: &Rcu<Payload>, ledger: &Ledger, index: usize) {
    let mut iteration: usize = 0;
    while !ledger.should_stop.load(Ordering::Relaxed) {
        crate::with_read(|scope| {
            let borrowed = value.read(scope);
            if borrowed.id != 0 {
                ledger.updated_reads.fetch_add(1, Ordering::Relaxed);
            }
            let mut is_intact = borrowed.is_intact();
            // Vary bounded read lengths without a handshake that would
            // serialize publication with the protected load.
            for _ in 0..(1 + iteration.wrapping_add(index) % 64) {
                core::hint::spin_loop();
            }
            crate::with_read(|nested| {
                is_intact &= value.read(nested).is_intact();
            });
            is_intact &= borrowed.is_intact();
            if !is_intact {
                ledger.errors.fetch_add(1, Ordering::Relaxed);
            }
        });
        ledger.reads[index].fetch_add(1, Ordering::Relaxed);
        iteration = iteration.wrapping_add(1);
        ktask::yield_now();
    }
}

fn run_writer(value: &Rcu<Payload>, ledger: &Arc<Ledger>, writer_index: usize) {
    for update in 0..UPDATES_PER_WRITER {
        let id = 1 + writer_index * UPDATES_PER_WRITER + update;
        match value.replace_sync(Payload::new(id, ledger)) {
            Ok(old) => {
                ledger.returned[old.id].fetch_add(1, Ordering::Relaxed);
                drop(old);
            }
            Err((_error, unused)) => {
                ledger.errors.fetch_add(1, Ordering::Relaxed);
                drop(unused);
                break;
            }
        }
    }
}

fn run_batch() {
    // Readers only delay for finite work and yield outside their scope. The
    // same workload can therefore run on UP without relying on preemption.
    let reader_count = kcpu_id_map::nr_cpus().max(2);
    let ledger = Arc::new(Ledger {
        drops: core::array::from_fn(|_| AtomicUsize::new(0)),
        returned: core::array::from_fn(|_| AtomicUsize::new(0)),
        reads: (0..reader_count).map(|_| AtomicUsize::new(0)).collect(),
        updated_reads: AtomicUsize::new(0),
        errors: AtomicUsize::new(0),
        should_stop: AtomicBool::new(false),
    });
    let value = Arc::new(Rcu::new(Payload::new(0, &ledger)));
    let mut readers = Vec::new();
    for index in 0..reader_count {
        let value = value.clone();
        let ledger = ledger.clone();
        let reader = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread(
            move || run_reader(&value, &ledger, index),
            "rcu-torture-reader".into(),
            kbuild_config::TASK_STACK_SIZE,
        ));
        ktask::activate_task(&reader);
        readers.push(reader);
    }
    // Require every reader to run, rather than accepting a zero-read pass.
    // The external bounded QEMU runner diagnoses a scheduling/service stall;
    // a timeout never grants permission to free a still-borrowed object.
    while ledger
        .reads
        .iter()
        .any(|count| count.load(Ordering::Relaxed) == 0)
    {
        ktask::yield_now();
    }
    let mut writers = Vec::new();
    for writer_index in 0..WRITERS {
        let value = value.clone();
        let ledger = ledger.clone();
        let writer = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread(
            move || run_writer(&value, &ledger, writer_index),
            "rcu-torture-writer".into(),
            kbuild_config::TASK_STACK_SIZE,
        ));
        ktask::activate_task(&writer);
        writers.push(writer);
    }
    let writer_statuses: Vec<_> = writers.iter().map(|task| task.join()).collect();
    ledger.should_stop.store(true, Ordering::Relaxed);
    let reader_statuses: Vec<_> = readers.iter().map(|task| task.join()).collect();
    let final_id = value.with_read(|payload| payload.id);
    drop(value);
    let reads: Vec<_> = ledger
        .reads
        .iter()
        .map(|count| count.load(Ordering::Relaxed))
        .collect();
    let retired: usize = ledger
        .returned
        .iter()
        .map(|count| count.load(Ordering::Relaxed))
        .sum();
    let freed: usize = ledger
        .drops
        .iter()
        .map(|count| count.load(Ordering::Relaxed))
        .sum();
    let errors = ledger.errors.load(Ordering::Relaxed);
    let updated_reads = ledger.updated_reads.load(Ordering::Relaxed);
    klogger::kprintln!(
        "RCU_TORTURE_BATCH readers={:?} updated_reads={} writers={} retired={} freed={}/{} \
         errors={}",
        reads,
        updated_reads,
        WRITERS,
        retired,
        freed,
        OBJECTS,
        errors
    );
    assert!(
        writer_statuses
            .iter()
            .chain(&reader_statuses)
            .all(|status| *status == 0)
    );
    assert!(reads.iter().all(|count| *count > 0));
    assert!(updated_reads > 0);
    assert_eq!(errors, 0);
    assert_eq!(retired, OBJECTS - 1);
    assert_eq!(freed, OBJECTS);
    for id in 0..OBJECTS {
        assert_eq!(ledger.drops[id].load(Ordering::Relaxed), 1);
        assert_eq!(
            ledger.returned[id].load(Ordering::Relaxed),
            usize::from(id != final_id)
        );
    }
}

/// Runs finite concurrent batches for at least the requested duration.
/// Call once in sleepable boot context after all RCU services are ready.
/// Each batch joins its workers and releases all owned publications.
///
/// # Panics
/// Panics on invalid duration, failed joins, content/ownership errors or lack
/// of observed reads. An external timeout must diagnose a stalled batch.
pub fn run(seconds: usize) {
    assert!((1..=86400).contains(&seconds));
    retirement::run();
    let begin = khal::time::monotonic_time_nanos();
    let deadline = begin + seconds as u64 * 1_000_000_000;
    let mut batches = 0;
    klogger::kprintln!(
        "RCU_TORTURE_BEGIN cpus={} seconds={seconds} coverage=false",
        kcpu_id_map::nr_cpus()
    );
    loop {
        run_batch();
        callbacks::run();
        batches += 1;
        if khal::time::monotonic_time_nanos() >= deadline {
            break;
        }
    }
    klogger::kprintln!(
        "RCU_TORTURE_PASS batches={batches} elapsed_ns={}",
        khal::time::monotonic_time_nanos() - begin
    );
}

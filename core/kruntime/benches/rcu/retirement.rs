// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Fixed storage makes an erroneous retirement observable without freeing memory.

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const REPLACEMENTS: usize = 64;

struct RetirementProbe {
    published: AtomicUsize,
    is_retired: [AtomicBool; 2],
}

impl RetirementProbe {
    fn new() -> Self {
        Self {
            published: AtomicUsize::new(0),
            is_retired: [AtomicBool::new(false), AtomicBool::new(true)],
        }
    }

    fn read_is_retired(&self, slot: usize) -> bool {
        self.is_retired[slot].load(Ordering::Relaxed)
    }
}

fn check_negative_control() {
    let probe = RetirementProbe::new();
    krcu::with_read(|_| {
        let slot = probe.published.load(Ordering::Acquire);
        assert!(!probe.read_is_retired(slot));
        // Deliberately simulate premature retirement, not a broken production
        // GP. The storage stays live and all mutable fields are atomic, so
        // testing the forbidden observation cannot itself cause a data race/UAF.
        probe.is_retired[slot].store(true, Ordering::Relaxed);
        let detected = probe.read_is_retired(slot);
        assert!(detected, "retirement oracle missed its injected fault");
    });
    klogger::kprintln!("RCU_RETIREMENT_NEGATIVE_CONTROL detected=1 expected=1");
}

/// Checks simulated premature retirement and then the real GP protocol.
pub(super) fn run() {
    check_negative_control();
    let probe = Arc::new(RetirementProbe::new());
    let should_stop = Arc::new(AtomicBool::new(false));
    let errors = Arc::new(AtomicUsize::new(0));
    let reader_count = kcpu_id_map::nr_cpus().max(2);
    let reads = Arc::new(
        (0..reader_count)
            .map(|_| AtomicUsize::new(0))
            .collect::<Vec<_>>(),
    );
    let mut readers = Vec::new();
    for index in 0..reader_count {
        let probe = probe.clone();
        let should_stop = should_stop.clone();
        let errors = errors.clone();
        let reads = reads.clone();
        let task = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread(
            move || {
                while !should_stop.load(Ordering::Relaxed) {
                    krcu::with_read(|_| {
                        let slot = probe.published.load(Ordering::Acquire);
                        let mut invalid = probe.read_is_retired(slot);
                        for _ in 0..64 {
                            core::hint::spin_loop();
                        }
                        invalid |= probe.read_is_retired(slot);
                        if invalid {
                            errors.fetch_add(1, Ordering::Relaxed);
                        }
                    });
                    reads[index].fetch_add(1, Ordering::Relaxed);
                    ktask::yield_now();
                }
            },
            "rcu-retirement-reader".into(),
            kbuild_config::TASK_STACK_SIZE,
        ));
        ktask::activate_task(&task);
        readers.push(task);
    }
    while reads.iter().any(|count| count.load(Ordering::Relaxed) == 0) {
        ktask::yield_now();
    }
    let mut completed = 0;
    for sequence in 0..REPLACEMENTS {
        let next = (sequence + 1) % 2;
        // This slot's previous GP finished before reuse. Readers can select it
        // again only after the release publication, which exposes this reset.
        probe.is_retired[next].store(false, Ordering::Relaxed);
        let old = probe.published.swap(next, Ordering::AcqRel);
        // SAFETY: watched sleepable boot task, no held locks. All selections
        // use the real host read scope; swap withdraws the only selection route.
        // Both slots remain allocated through all waits and reader joins.
        if unsafe { krcu::synchronize_rcu() }.is_err() {
            errors.fetch_add(1, Ordering::Relaxed);
            break;
        }
        probe.is_retired[old].store(true, Ordering::Relaxed);
        completed += 1;
    }
    should_stop.store(true, Ordering::Relaxed);
    let statuses: Vec<_> = readers.iter().map(|task| task.join()).collect();
    let counts: Vec<_> = reads
        .iter()
        .map(|count| count.load(Ordering::Relaxed))
        .collect();
    let errors = errors.load(Ordering::Relaxed);
    klogger::kprintln!(
        "RCU_RETIREMENT_RESULT reads={counts:?} completed={completed} errors={errors}"
    );
    assert!(statuses.iter().all(|status| *status == 0));
    assert!(counts.iter().all(|count| *count > 0));
    assert_eq!(completed, REPLACEMENTS);
    assert_eq!(errors, 0);
}

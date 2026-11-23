// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-Line receive counters owned by KIRQ.
//!
//! A hardware claim records once on its claiming CPU before action fanout.
//! Replay and simulated entries never reach this type. The array is bounded by
//! the compile-time CPU capacity; no per-IRQ allocation or global lock exists.

use core::sync::atomic::{AtomicU64, Ordering};

/// Receive counters for one Line, indexed by logical CPU identity.
pub(super) struct LineStats {
    counts: [AtomicU64; kbuild_config::NR_CPUS],
}

impl LineStats {
    pub(super) fn new() -> Self {
        Self {
            counts: core::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    /// Records one received hardware claim on `cpu`.
    ///
    /// The caller has already validated the Line identity and ordinary flow.
    /// Out-of-range CPUs are a broken entry contract; do not wrap the index.
    pub(super) fn record_receive(&self, cpu: usize) {
        if let Some(counter) = self.counts.get(cpu) {
            counter.fetch_add(1, Ordering::Relaxed);
        } else {
            debug_assert!(false, "IRQ receive on a CPU outside NR_CPUS");
        }
    }

    /// Copies all counters without claiming a cross-CPU atomic instant.
    pub(super) fn copy_counts(&self) -> [u64; kbuild_config::NR_CPUS] {
        core::array::from_fn(|cpu| self.counts[cpu].load(Ordering::Relaxed))
    }
}

#[cfg(unittest)]
impl LineStats {
    pub(super) fn set_count_for_tests(&self, cpu: usize, value: u64) {
        self.counts[cpu].store(value, Ordering::Relaxed);
    }
}

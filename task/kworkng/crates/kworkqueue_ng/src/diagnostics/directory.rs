// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Weak diagnostic discovery; no queue retention or deferred GC protocol.

use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
    vec::Vec,
};

use kcpu_id_map::LogicalCpuId;
use ksync::Mutex;
use ktime_types::{MonotonicInstant, TimeSpan};

use super::record::StallRecord;

/// Task-context registry. Creation can allocate; monitoring skips a busy registry.
/// Partitions never acquire this mutex during submission, execution or destruction.
pub struct Directory {
    records: Mutex<Vec<Weak<StallRecord>>>,
}

impl Directory {
    /// Creates an empty task-context diagnostic registry.
    pub fn new() -> Self {
        Self {
            records: Mutex::new(Vec::new()),
        }
    }

    /// Prepare before queue exposure in sleepable task context. Dead weak slots
    /// are removed here as well as during inspection, bounding creation-only churn.
    pub(crate) fn register(&self, name: Box<str>, cpu: LogicalCpuId) -> Arc<StallRecord> {
        let record = Arc::new(StallRecord::new(name, cpu));
        let mut records = self.records.lock();
        records.retain(|entry| entry.strong_count() != 0);
        records.push(Arc::downgrade(&record));
        record
    }

    /// Best-effort periodic task-context check, never an IRQ/NMI check. A busy
    /// directory skips this pass; a busy record skips only that record. False means
    /// no pass occurred, not that the queues are healthy. No deadline is promised.
    ///
    /// A zero threshold suppresses all warnings, not just throttling. Inspection
    /// still prunes expired records and returns true if the directory was scanned;
    /// true does not mean warning generation is enabled. A zero-threshold pass
    /// does not consume warning opportunities for a later nonzero threshold.
    ///
    /// report runs after record protection ends, but under the sleepable registry
    /// mutex. It must only emit diagnostics, never register queues or reenter this
    /// directory. Temporary references own diagnostic data only; dropping them can
    /// free that data in this task, never run Work callbacks or queue teardown.
    ///
    /// # Panics
    /// Propagates a panic from the supplied reporting callback.
    pub fn inspect(
        &self,
        cpu: LogicalCpuId,
        now: MonotonicInstant,
        threshold: TimeSpan,
        mut report: impl FnMut(&str, LogicalCpuId, TimeSpan),
    ) -> bool {
        let Some(mut records) = self.records.try_lock() else {
            return false;
        };
        records.retain(|entry| {
            let Some(record) = entry.upgrade() else {
                return false;
            };
            if record.cpu == cpu
                && let Some(elapsed) = record.take_warning(now, threshold)
            {
                report(&record.name, record.cpu, elapsed);
            }
            true
        });
        true
    }
}

impl Default for Directory {
    fn default() -> Self {
        Self::new()
    }
}

unittest::path_mod_test!(
    directory_unittest,
    "../../ut-tests/diagnostics_directory_unittest.rs"
);

unittest::path_mod_test!(
    directory_partition_unittest,
    "../../ut-tests/diagnostics_directory_partition_unittest.rs"
);

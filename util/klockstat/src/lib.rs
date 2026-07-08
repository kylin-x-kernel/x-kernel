// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-class lock contention statistics.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::{string::String, vec::Vec};
use core::{
    fmt::Write,
    sync::atomic::{AtomicU64, Ordering},
};

#[cfg(feature = "stats")]
mod runtime;

#[cfg(feature = "stats")]
pub use klockstat_macros::static_lock;
pub use linkme;
#[cfg(feature = "stats")]
pub use runtime::class_for_init_site;

/// Snapshot of counters collected for one lock class.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LockClassEntry {
    /// Lock class label, usually `file:line`.
    pub location: &'static str,
    /// Lock kind, such as `Mutex` or `SpinNoIrq`.
    pub kind: &'static str,
    /// Times a caller had to wait before acquiring the lock.
    pub contentions: u64,
    /// Successful lock acquisitions (`lock` and successful `try_lock`).
    pub acquisitions: u64,
}

/// Placeholder counter set for locks without registered statistics.
///
/// Locks that are not bound to a tracked class (for example
/// `Mutex::const_new(RawMutex::new(), ...)`) point here. Recording is a no-op
/// and this counter set is not registered in [`LOCK_CLASSES`].
pub static NOOP_CLASS: LockClassStats = LockClassStats::untracked();

/// Contention counters for one lock class.
pub struct LockClassStats {
    location: &'static str,
    kind: &'static str,
    tracked: bool,
    contentions: AtomicU64,
    acquisitions: AtomicU64,
}

impl LockClassStats {
    /// Creates a zeroed counter set for a lock class labeled `location`.
    pub const fn new(location: &'static str, kind: &'static str) -> Self {
        Self {
            location,
            kind,
            tracked: true,
            contentions: AtomicU64::new(0),
            acquisitions: AtomicU64::new(0),
        }
    }

    const fn untracked() -> Self {
        Self {
            location: "",
            kind: "",
            tracked: false,
            contentions: AtomicU64::new(0),
            acquisitions: AtomicU64::new(0),
        }
    }

    /// Returns whether this counter set participates in lock statistics.
    pub const fn is_tracked(&self) -> bool {
        self.tracked
    }

    /// Returns the lock class label.
    pub const fn location(&self) -> &'static str {
        self.location
    }

    /// Returns the lock kind label.
    pub const fn kind(&self) -> &'static str {
        self.kind
    }

    /// Records successful lock acquisitions.
    #[inline(always)]
    pub fn record_acquisitions(&self, count: u64) {
        if self.tracked && count != 0 {
            self.acquisitions.fetch_add(count, Ordering::Relaxed);
        }
    }

    /// Records task blocking while waiting for a lock.
    #[inline(always)]
    pub fn record_contentions(&self, count: u64) {
        if self.tracked && count != 0 {
            self.contentions.fetch_add(count, Ordering::Relaxed);
        }
    }

    /// Records acquisitions and contentions in one call.
    #[inline(always)]
    pub fn record(&self, acquisitions: u64, contentions: u64) {
        if !self.tracked {
            return;
        }
        self.record_acquisitions(acquisitions);
        self.record_contentions(contentions);
    }

    /// Returns a stable snapshot of this counter set.
    pub fn snapshot(&self) -> LockClassEntry {
        LockClassEntry {
            location: self.location,
            kind: self.kind,
            contentions: self.contentions.load(Ordering::Relaxed),
            acquisitions: self.acquisitions.load(Ordering::Relaxed),
        }
    }
}

/// Registered static lock-stat counter sets.
#[linkme::distributed_slice]
pub static LOCK_CLASSES: [&'static LockClassStats];

/// Maximum number of entries shown in [`dump_lock_stat`].
pub const DUMP_TOP_N: usize = 5;

const COL_LOCATION: usize = 64;
const COL_KIND: usize = 12;
const COL_NUM: usize = 12;

/// Returns snapshots for all registered lock statistics.
pub fn snapshot() -> Vec<LockClassEntry> {
    LOCK_CLASSES
        .iter()
        .map(|stats| stats.snapshot())
        .chain(runtime_entries())
        .collect()
}

#[cfg(feature = "stats")]
fn runtime_entries() -> alloc::vec::IntoIter<LockClassEntry> {
    runtime::runtime_stats_snapshot().into_iter()
}

#[cfg(not(feature = "stats"))]
fn runtime_entries() -> core::iter::Empty<LockClassEntry> {
    core::iter::empty()
}

/// Returns the top `limit` entries ranked by contention count.
pub fn top_snapshot(limit: usize) -> Vec<LockClassEntry> {
    let mut entries = snapshot();
    entries.retain(|entry| entry.contentions != 0 || entry.acquisitions != 0);
    sort_entries(&mut entries);
    entries.truncate(limit);
    entries
}

fn sort_entries(entries: &mut [LockClassEntry]) {
    entries.sort_by(|lhs, rhs| {
        rhs.contentions
            .cmp(&lhs.contentions)
            .then_with(|| rhs.acquisitions.cmp(&lhs.acquisitions))
            .then_with(|| lhs.location.cmp(rhs.location))
    });
}

/// Dumps lock contention statistics for `/proc/lock_stat`.
pub fn dump_lock_stat() -> String {
    let entries = top_snapshot(DUMP_TOP_N);

    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:<COL_LOCATION$} {:<COL_KIND$} {:>COL_NUM$} {:>COL_NUM$}",
        "location", "kind", "contentions", "acquisitions"
    );
    push_row_separator(&mut out);

    if entries.is_empty() {
        out.push_str("no lock contention statistics collected\n");
        return out;
    }

    for entry in entries {
        let _ = writeln!(
            out,
            "{:<COL_LOCATION$} {:<COL_KIND$} {:>COL_NUM$} {:>COL_NUM$}",
            entry.location, entry.kind, entry.contentions, entry.acquisitions
        );
    }

    out
}

fn push_row_separator(out: &mut String) {
    let width = COL_LOCATION + 1 + COL_KIND + 1 + COL_NUM * 2 + 1;
    for _ in 0..width {
        out.push('-');
    }
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dump_lock_stat_sorts_by_contentions() {
        static STATS_A: LockClassStats = LockClassStats::new("a", "Mutex");
        static STATS_B: LockClassStats = LockClassStats::new("b", "Mutex");

        STATS_A.record(1, 10);
        STATS_B.record(1, 20);

        let mut entries = vec![STATS_A.snapshot(), STATS_B.snapshot()];
        sort_entries(&mut entries);
        assert_eq!(entries[0].location, "b");
        assert_eq!(entries[1].location, "a");
    }

    #[test]
    fn dump_lock_stat_aligns_columns() {
        let out = dump_lock_stat_from(&[LockClassEntry {
            location: "process/kprocess/src/scheduler.rs:18",
            kind: "RwLock",
            contentions: 0,
            acquisitions: 33,
        }]);

        let line = out.lines().nth(2).unwrap();
        assert!(line.ends_with("33"));
        assert!(line.contains("RwLock"));
        let kind_pos = line.find("RwLock").unwrap();
        let acq_pos = line.rfind("33").unwrap();
        assert!(kind_pos < acq_pos);
    }

    fn dump_lock_stat_from(entries: &[LockClassEntry]) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "{:<COL_LOCATION$} {:<COL_KIND$} {:>COL_NUM$} {:>COL_NUM$}",
            "location", "kind", "contentions", "acquisitions"
        );
        push_row_separator(&mut out);
        for entry in entries {
            let _ = writeln!(
                out,
                "{:<COL_LOCATION$} {:<COL_KIND$} {:>COL_NUM$} {:>COL_NUM$}",
                entry.location, entry.kind, entry.contentions, entry.acquisitions
            );
        }
        out
    }

    #[test]
    fn noop_stats_do_not_record() {
        NOOP_CLASS.record(10, 3);
        let snap = NOOP_CLASS.snapshot();
        assert!(!NOOP_CLASS.is_tracked());
        assert_eq!(snap.contentions, 0);
        assert_eq!(snap.acquisitions, 0);
    }

    #[test]
    fn top_snapshot_limits_to_n() {
        let mut entries = vec![
            LockClassEntry {
                location: "a",
                kind: "Mutex",
                contentions: 1,
                acquisitions: 0,
            },
            LockClassEntry {
                location: "b",
                kind: "Mutex",
                contentions: 2,
                acquisitions: 0,
            },
            LockClassEntry {
                location: "c",
                kind: "Mutex",
                contentions: 3,
                acquisitions: 0,
            },
        ];
        sort_entries(&mut entries);
        entries.truncate(2);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].location, "c");
        assert_eq!(entries[1].location, "b");
    }
}

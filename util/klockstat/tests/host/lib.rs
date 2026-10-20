// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

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

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use lock_api::RawMutex;
use unittest::def_test;

use super::TraceRawLock;

#[def_test]
fn trace_raw_lock_try_lock_tracks_state_and_unlocks() {
    let lock = TraceRawLock::INIT;

    assert!(!lock.is_locked());
    assert!(lock.try_lock());
    assert!(lock.is_locked());
    assert!(!lock.try_lock());

    // SAFETY: the lock above was acquired successfully and has not yet been unlocked.
    unsafe { lock.unlock() };

    assert!(!lock.is_locked());
    assert!(lock.try_lock());
    // SAFETY: the second acquisition above succeeded and is released here.
    unsafe { lock.unlock() };
}

#[def_test]
fn trace_raw_lock_lock_path_matches_try_lock_state() {
    let lock = TraceRawLock::INIT;

    lock.lock();
    assert!(lock.is_locked());
    assert!(!lock.try_lock());

    // SAFETY: `lock()` above acquired the raw lock and intentionally leaked the guard.
    unsafe { lock.unlock() };

    assert!(!lock.is_locked());
}

#[def_test]
fn trace_header_preserves_upstream_columns_and_snapshot_counts() {
    for count in [0, 1, 3] {
        let records = alloc::vec![alloc::vec![]; count];
        assert_eq!(
            super::trace_header(count),
            ktracepoint::TracePipeSnapshot::new(records).default_fmt_str()
        );
    }
}

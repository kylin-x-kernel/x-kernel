// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

pub use crate::process::{
    WaitChildKind, WaitChildScan, WaitChildSelector, WaitReapMode, WaitedChild,
};
use crate::{Pid, Process, lookup};

/// Records a reaped child's CPU time into its parent.
pub fn record_reaped_child_cpu_time(
    parent: &Process,
    utime: ktime_types::TimeSpan,
    stime: ktime_types::TimeSpan,
) {
    parent.accumulate_child_time(utime, stime);
}

/// Releases a zombie process from its parent relation and PID directory.
///
/// Returns `true` only when this call successfully consumes the waitable
/// zombie. Racing callers should treat `false` as "someone else already
/// consumed or detached it" and rescan their wait condition.
pub fn reap_zombie_process(process: &Arc<Process>) -> bool {
    try_reap_zombie_process(process)
}

/// Reaps a zombie process and panics if the invariant does not hold.
///
/// This is intended for tests and invariant-checked internal paths. Racing
/// wait paths should use [`reap_zombie_process`] or [`try_reap_zombie_process`].
#[cfg(unittest)]
pub fn assert_reap_zombie_process(process: &Arc<Process>) {
    assert!(
        reap_zombie_process(process),
        "process {} must be a waitable zombie linked to its parent",
        process.pid()
    );
}

/// Tries to reap a zombie process exactly once.
///
/// Returns `true` only for the waiter that successfully detached the zombie
/// from the parent's live child relation.
pub fn try_reap_zombie_process(process: &Arc<Process>) -> bool {
    if !process.reap_waitable_zombie_from_parent() {
        return false;
    }
    lookup::unpublish_process_if_matches(process);
    true
}

/// Scans `parent` for a matching waitable child and optionally consumes it.
///
/// The child relation and zombie consumption decision are resolved inside the
/// process-domain transaction boundary. If a child is consumed, its PID identity
/// is removed from the publication directory after the tree relation has been
/// detached.
pub fn scan_waitable_child(
    parent: &Arc<Process>,
    selector: WaitChildSelector,
    kind: WaitChildKind,
    mode: WaitReapMode,
) -> WaitChildScan {
    let scan = parent.scan_waitable_child(selector, kind, mode);
    if let WaitChildScan::Ready(waited) = &scan
        && waited.was_consumed()
    {
        let (utime, stime) = waited.total_cpu_time();
        record_reaped_child_cpu_time(parent, utime, stime);
    }
    scan
}

/// Releases a process that has already transitioned out of the waitable state.
pub fn reap_exited_process(process: &Arc<Process>) {
    if process.detach_dead_from_parent() {
        lookup::unpublish_process_if_matches(process);
    }
}

/// Removes a process identity after the process-domain relation was already detached.
pub fn reap_detached_process_identity(process: &Arc<Process>) {
    lookup::unpublish_process_if_matches(process);
}

/// Removes a reaped process identity from the global PID directory.
pub fn reap_process_identity(pid: Pid) {
    lookup::unpublish_process(pid);
}

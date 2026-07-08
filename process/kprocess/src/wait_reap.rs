// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use crate::{Pid, Process, lookup};

/// Records a reaped child's CPU time into its parent.
pub fn record_reaped_child_cpu_time(parent: &Process, utime_ns: usize, stime_ns: usize) {
    parent.accumulate_child_time(utime_ns, stime_ns);
}

/// Releases a zombie process from its parent relation and PID directory.
pub fn reap_zombie_process(process: &Arc<Process>) {
    process.free();
    lookup::unpublish_process(process.pid());
}

/// Tries to reap a zombie process exactly once.
///
/// Returns `true` only for the waiter that successfully detached the zombie
/// from the parent's live child relation.
pub fn try_reap_zombie_process(process: &Arc<Process>) -> bool {
    if !process.try_detach_from_parent() {
        return false;
    }
    lookup::unpublish_process(process.pid());
    true
}

/// Removes a reaped process identity from the global PID directory.
pub fn reap_process_identity(pid: Pid) {
    lookup::unpublish_process(pid);
}

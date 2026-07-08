// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use crate::{Process, Tid};

/// Removes an exiting thread from its process and publishes the exit code when needed.
pub fn finish_thread_exit(process: &Arc<Process>, tid: Tid, exit_code: i32) -> bool {
    process.exit_thread(tid, exit_code)
}

/// Marks the process thread group as group-exited.
pub fn mark_group_exited(process: &Process) {
    process.group_exit();
}

/// Records CPU time accumulated by a thread that has just exited.
pub fn record_exited_thread_cpu_time(process: &Process, utime_ns: usize, stime_ns: usize) {
    process.accumulate_exited_thread_time(utime_ns, stime_ns);
}

/// Transitions the process into zombie state and reparents surviving children.
pub fn finalize_process_exit(process: &Arc<Process>) {
    process.exit();
}

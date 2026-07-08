// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use kerrno::{KError, KResult};
use ktask::{KtaskRef, current};

use crate::{AsThread, Pid, Process, ProcessGroup, Tid, lookup};

/// Resolves the task targeted by scheduler syscalls.
pub fn target_task(pid: i32) -> KResult<KtaskRef> {
    if pid < 0 {
        return Err(KError::NoSuchProcess);
    }
    if pid == 0 {
        return Ok(current().clone());
    }

    let pid = pid as u32;
    let current_task = current();
    if let Some(thread) = current_task.try_as_thread()
        && thread.pid() == pid
    {
        return Ok(current_task.clone());
    }

    let process = lookup::live_process(pid)?;
    representative_task(process.as_ref())
}

/// Resolves a thread task by TID for scheduler attribute updates.
pub fn task_by_tid(tid: Tid) -> KResult<KtaskRef> {
    lookup::task(tid)
}

/// Returns the published tasks that currently belong to the selected process.
pub fn process_tasks(process: &Process) -> alloc::vec::Vec<KtaskRef> {
    lookup::process_tasks(process)
}

/// Returns the number of published tasks that currently belong to the selected process.
pub fn process_task_count(process: &Process) -> usize {
    process.thread_count()
}

/// Returns whether the selected TID currently resolves to a published thread in `process`.
pub fn process_owns_tid(process: &Process, tid: Tid) -> bool {
    lookup::process_has_published_tid(process, tid)
}

/// Resolves a representative published task for the selected process.
pub fn representative_task(process: &Process) -> KResult<KtaskRef> {
    lookup::representative_task_for_process(process)
}

/// Resolves the non-zombie process targeted by scheduler process-level operations.
pub fn target_process(pid: Pid) -> KResult<Arc<Process>> {
    lookup::live_process(pid)
}

/// Lists non-zombie processes that participate in scheduler-wide scans.
pub fn processes() -> alloc::vec::Vec<Arc<Process>> {
    lookup::live_processes()
}

/// Resolves the process group targeted by scheduler group scans.
pub fn target_group(pgid: Pid) -> KResult<Arc<ProcessGroup>> {
    lookup::process_group(pgid)
}

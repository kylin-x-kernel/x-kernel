// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use kerrno::{KError, KResult};
use ktask::KtaskRef;

use crate::{Pid, Process, Tid, lookup};

fn has_representative_task(process: &Arc<Process>) -> bool {
    lookup::representative_task_for_process(process).is_ok()
}

/// Returns the process identities that should appear in `/proc`.
pub fn visible_processes() -> alloc::vec::Vec<Arc<Process>> {
    lookup::published_processes()
        .into_iter()
        .filter(has_representative_task)
        .collect()
}

/// Resolves the representative task used for a `/proc/<pid>` lookup.
///
/// # Errors
///
/// Returns `kerrno::KError::NoSuchProcess` when the requested published target
/// is absent or no longer has the required live backing.
pub fn process_task(pid: Pid) -> KResult<KtaskRef> {
    let process = lookup::published_process(pid)?;
    lookup::representative_task_for_process(&process).map_err(|_| KError::NoSuchProcess)
}

/// Resolves a thread task used for `/proc/<pid>/task/<tid>` lookups.
///
/// # Errors
///
/// Returns `kerrno::KError::NoSuchProcess` when a nonzero TID has no live
/// published task. TID zero selects the current task.
pub fn thread_task(tid: Tid) -> KResult<KtaskRef> {
    lookup::task(tid)
}

/// Returns the published thread IDs currently visible under `/proc/<pid>/task`.
pub fn thread_ids(process: &Arc<Process>) -> alloc::vec::Vec<Tid> {
    process.threads()
}

/// Returns unreleased thread-group members for task-directory link counts.
///
/// Includes an exited group leader until wait/autoreap consumes its process
/// identity. Nonleader exec does not transfer canonical identity or membership
/// here; its Linux-exact post-exec count requires a unified takeover transition.
/// Unlike `scheduler::process_task_count`, this is a lifetime query,
/// not a count of schedulable or globally discoverable tasks. It allocates no
/// list, but scans stored member slots under their existing spinlocks; slot
/// publication may interleave, so the result is not an atomic group snapshot.
pub fn thread_group_member_count(process: &Process) -> usize {
    process.unreleased_thread_count()
}

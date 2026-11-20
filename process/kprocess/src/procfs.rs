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

/// Returns the processes visible from `observer_ns`, or every process when
/// `None` (the initial-namespace view).
///
/// A process appears when its group leader holds a PID number in the
/// observer's namespace — the same rule Linux applies when a pid-namespace-
/// scoped procfs instance lists its directory.
pub fn visible_processes_in_namespace(
    observer_ns: Option<&Arc<crate::PidNamespace>>,
) -> alloc::vec::Vec<Arc<Process>> {
    let Some(observer_ns) = observer_ns else {
        return visible_processes();
    };
    lookup::published_processes()
        .into_iter()
        .filter(|process| {
            process.number_in_namespace(observer_ns).is_some() && has_representative_task(process)
        })
        .collect()
}

/// Resolves a `/proc/<pid>` target visible from `observer_ns`.
///
/// # Errors
///
/// Returns `kerrno::KError::NoSuchProcess` when the PID does not resolve in
/// the observer namespace or has no live backing.
pub fn process_task_in_namespace(
    pid: Pid,
    observer_ns: Option<&Arc<crate::PidNamespace>>,
) -> KResult<KtaskRef> {
    let Some(observer_ns) = observer_ns else {
        return process_task(pid);
    };
    let process = lookup::published_process_in_namespace(pid, observer_ns)?;
    lookup::representative_task_for_process(&process).map_err(|_| KError::NoSuchProcess)
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
/// identity, or until a surviving thread takes over the group identity by a
/// successful non-leader `exec`; that takeover drops the retired leader slot.
/// Unlike `scheduler::process_task_count`, this is a lifetime query,
/// not a count of schedulable or globally discoverable tasks. It allocates no
/// list, but scans stored member slots under their existing spinlocks; slot
/// publication may interleave, so the result is not an atomic group snapshot.
pub fn thread_group_member_count(process: &Process) -> usize {
    process.unreleased_thread_count()
}

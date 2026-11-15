// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{collections::BTreeSet, sync::Arc, vec::Vec};

use kcgroup::Cgroup;
use kerrno::KResult;

use crate::{AsThread, Pid, current_user_process, publication::process_publication, scheduler};

/// Collects root-namespace process IDs directly represented in `cgroup`.
///
/// Membership is keyed by the stable identity token, so this resolves each
/// token to its live task and reads the current process id on demand; a stale
/// or exited member is omitted.
pub fn cgroup_member_process_ids(cgroup: &Cgroup) -> Vec<Pid> {
    let mut process_ids = BTreeSet::new();
    let publication = process_publication();
    for token in cgroup.member_tasks() {
        if let Some(task) = publication.task_by_identity_token(token) {
            process_ids.insert(task.as_thread().process().pid());
        }
    }
    process_ids.into_iter().collect()
}

/// Resolves and migrates a process after filesystem-owned authorization.
///
/// A zero process ID selects the current process. The authorization callback
/// observes the stable source and destination while the process cgroup gate is
/// held, before any membership changes are committed.
///
/// # Errors
///
/// Returns the lookup, authorization, or atomic group-migration error.
pub fn migrate_cgroup_process(
    pid: Pid,
    destination: &Arc<Cgroup>,
    authorize: impl FnOnce(&Arc<Cgroup>, &Arc<Cgroup>) -> KResult<()>,
) -> KResult<()> {
    let process = if pid == 0 {
        current_user_process()
    } else {
        scheduler::target_process(pid)?
    };
    scheduler::migrate_process_cgroup_with(&process, destination, authorize)
}

/// Returns the cgroup the current user thread belongs to.
///
/// Execution context (CG-039): callable from any task context, kernel tasks
/// included — it reads the current task pointer and clones the thread's
/// cgroup `Arc`, so it never sleeps, is safe from IRQ context, and permits
/// any caller-held locks. Kernel tasks and any task that never joined a
/// cgroup yield `None`, which callers treat as "no cgroup policy applies";
/// the function never panics.
pub fn current_cgroup() -> Option<Arc<Cgroup>> {
    ktask::current()
        .try_as_thread()
        .and_then(|thread| thread.cgroup())
}

/// Returns the memory cgroup of the current task's cgroup membership.
///
/// The membership lock is an IRQ-safe spinlock, so this is safe from page
/// allocation and interrupt context. The lookup is live: enabling `+memory`
/// after a task joined a child cgroup is observed on the next charge.
pub fn current_memcg() -> memacct::MemcgId {
    current_cgroup()
        .map(|cgroup| cgroup.effective_memcg().id())
        .unwrap_or(memacct::MemcgId::NONE)
}

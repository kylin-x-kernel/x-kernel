// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{collections::BTreeSet, sync::Arc, vec::Vec};

use kcgroup::Cgroup;
use kerrno::KResult;

use crate::{AsThread, Pid, current_user_process, procfs, scheduler};

/// Collects root-namespace process IDs directly represented in `cgroup`.
///
/// Stale registry entries and task-number projections whose stable identity no
/// longer matches the cgroup membership are omitted.
pub fn cgroup_member_process_ids(cgroup: &Cgroup) -> Vec<Pid> {
    let mut process_ids = BTreeSet::new();
    for identity in cgroup.member_tasks() {
        let Ok(task) = procfs::thread_task(identity.root_nr()) else {
            continue;
        };
        if Arc::ptr_eq(task.as_thread().task_number(), &identity) {
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

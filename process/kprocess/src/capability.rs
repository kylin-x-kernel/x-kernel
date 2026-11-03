// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Capability lookup for another process.

use alloc::sync::Arc;

use kcred::CapSets;
use kerrno::{KError, KResult};
use kidentity::PidNamespace;

use crate::{AsThread, Pid, Process, lookup};

/// Returns the capability sets observed for `pid` in `observer_ns`.
///
/// PID zero selects the caller itself, matching Linux's special case in
/// `capget(2)`. Every positive PID is resolved in `observer_ns` — the caller's
/// active PID namespace — because Linux `find_task_by_vpid()` interprets the
/// argument there while the live indexes are root-keyed. A positive PID may
/// name either a thread or its thread-group leader.
///
/// The result is the target's objective real credential, exactly as
/// `cap_capget()` reads `__task_cred()`. A thread-group leader that has exited
/// while sibling threads remain may no longer be in the live task index.
/// Linux keeps the zombie leader, so this path reads the leader credentials
/// retained by the process identity rather than a surviving sibling's
/// per-thread credentials.
///
/// # Errors
///
/// Returns [`KError::NoSuchProcess`] when the PID does not name a published
/// user task or process identity visible in `observer_ns`. An unreaped zombie
/// remains published so its retained leader credentials stay observable;
/// reaping removes the identity.
pub fn target_capabilities(pid: Pid, observer_ns: &Arc<PidNamespace>) -> KResult<CapSets> {
    if pid == 0 {
        return Ok(crate::current_real_cred().capabilities());
    }

    // Resolve the identity first, then use its root number for the live-index
    // and retained-leader lookups below. The identity lookup covers member
    // threads as well as leaders; an invisible or missing number is `ESRCH`.
    let pid = if observer_ns.is_root() {
        pid
    } else {
        Process::pid_identity_in_namespace(pid, observer_ns)
            .map(|identity| identity.root_nr())
            .ok_or(KError::NoSuchProcess)?
    };

    if let Ok(task) = lookup::task(pid)
        && let Some(thread) = task.try_as_thread()
    {
        return Ok(thread.real_cred().capabilities());
    }

    let process = lookup::published_process(pid)?;
    if let Some(cred) = process.exited_leader_cred() {
        return Ok(cred.capabilities());
    }
    let task = lookup::representative_task_for_process(&process)?;
    let thread = task.try_as_thread().ok_or(KError::NoSuchProcess)?;
    Ok(thread.real_cred().capabilities())
}

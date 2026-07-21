// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use khal::time::ns2t;
use ksignal::{ChildExitInfo, ChildExitSignalInfo, SigchldChildExitSignalInfo, Signo};

use crate::{Process, ProcessExitPublication, Tid, process_signals, wait_reap};

/// Removes an exiting thread from its process and publishes the exit code when needed.
pub fn finish_thread_exit(process: &Arc<Process>, tid: Tid, exit_code: i32) -> bool {
    process.exit_thread(tid, exit_code)
}

/// Marks the process thread group as group-exited.
pub fn mark_group_exited(process: &Process) {
    process.group_exit();
}

/// Records CPU time accumulated by a thread that has just exited.
pub fn record_exited_thread_cpu_time(process: &Process, utime_ns: u64, stime_ns: u64) {
    process.accumulate_exited_thread_time(utime_ns, stime_ns);
}

/// Transitions the process into waitable zombie state and reparents surviving children.
pub fn finalize_process_exit(process: &Arc<Process>) {
    finalize_process_exit_with_publication(process, ProcessExitPublication::WaitableZombie);
}

/// Transitions the process into an exited state and reparents surviving children.
pub fn finalize_process_exit_with_publication(
    process: &Arc<Process>,
    publication: ProcessExitPublication,
) {
    process.exit_with_publication(publication);
}

/// Completes process exit after the last thread has finished runtime cleanup.
///
/// The parent wait event is the tail publication step: a parent woken by
/// `wait*()` should only observe a fully resolved child-exit state, either a
/// waitable zombie or an already-detached autoreaped child.
pub fn complete_process_exit(process: &Arc<Process>) -> bool {
    let transition =
        process.finish_exit_in_process_domain(ProcessExitPublication::WaitableZombie, |parent| {
            let siginfo = sigchld_child_exit_signal_info(process);
            process_signals::prepare_child_exit_to_process_ref(parent, siginfo)
                .map(|delivery| {
                    let autoreap = delivery.should_autoreap();
                    (delivery, autoreap)
                })
                .inspect_err(|err| {
                    warn!(
                        "failed to prepare child-exit SIGCHLD for child {} parent {}: {err:?}",
                        process.pid(),
                        parent.pid()
                    );
                })
                .ok()
        });

    if let Some(parent) = transition.reparented_zombie_parent {
        notify_child_exit(&parent);
    }
    if transition.autoreaped {
        wait_reap::reap_detached_process_identity(process);
    }

    let Some(parent) = transition.parent else {
        return false;
    };

    if let Some(delivery) = transition.prepared_sigchld {
        delivery.commit_and_interrupt();
    } else if let Some(signo) = transition.exit_signal
        && signo != Signo::SIGCHLD
        && let Err(err) = process_signals::send_to_process_ref(
            &parent,
            Some(child_exit_signal_info(process, signo).into_signal_info()),
        )
    {
        warn!(
            "failed to send child-exit signal {signo:?} for child {} parent {}: {err:?}",
            process.pid(),
            parent.pid()
        );
    }

    notify_child_exit(&parent);

    transition.autoreaped
}

/// Wakes parent waiters after child-exit signal/autoreap handling is complete.
pub fn notify_child_exit(parent: &Process) {
    parent.notify_child_exit();
}

pub(crate) fn child_exit_signal_info(process: &Process, signo: Signo) -> ChildExitSignalInfo {
    ChildExitSignalInfo::new(signo, child_exit_info(process))
}

pub(crate) fn sigchld_child_exit_signal_info(process: &Process) -> SigchldChildExitSignalInfo {
    ChildExitSignalInfo::new_sigchld(child_exit_info(process))
}

fn child_exit_info(process: &Process) -> ChildExitInfo {
    let (utime_ns, stime_ns) = process.process_cpu_time_ns();
    let uid = process
        .credentials_snapshot()
        .map(|credentials| credentials.ruid())
        .unwrap_or(0);
    ChildExitInfo::from_wait_status(
        process.pid(),
        uid,
        process.exit_code(),
        ns2t(utime_ns),
        ns2t(stime_ns),
    )
}

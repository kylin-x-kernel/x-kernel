// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process waiting and status syscalls.
//!
//! This module implements process status waiting operations including:
//! - Wait for process termination (wait, waitpid, waitid, etc.)
//! - Process status retrieval and interpretation
//! - Child process status monitoring

use alloc::sync::Arc;
#[cfg(unittest)]
use alloc::vec::Vec;
use core::{future::poll_fn, task::Poll};

use bitflags::bitflags;
use kerrno::{KError, KResult, LinuxError};
use kpoll::PollRegistrations;
use kprocess::{Pid, Process, wait_reap, wait_reap::WaitedChild};
use ktask::{current, future::block_on};
use linux_raw_sys::general::{
    __WALL, __WCLONE, __WNOTHREAD, P_ALL, P_PGID, P_PID, P_PIDFD, WCONTINUED, WEXITED, WNOHANG,
    WNOWAIT, WUNTRACED, siginfo_t,
};
use osvm::{VirtMutPtr, VirtPtr};
use posix_types::{UserConstPtr, UserPtr};

bitflags! {
    #[derive(Debug, Clone, Copy)]
    struct WaitOptions: u32 {
        /// Do not block when there are no processes wishing to report status.
        const WNOHANG = WNOHANG;
        /// Report the status of selected processes which are stopped due to a
        /// `SIGTTIN`, `SIGTTOU`, `SIGTSTP`, or `SIGSTOP` signal.
        const WUNTRACED = WUNTRACED;
        /// Report the status of selected processes which have terminated.
        const WEXITED = WEXITED;
        /// Report the status of selected processes that have continued from a
        /// job control stop by receiving a `SIGCONT` signal.
        const WCONTINUED = WCONTINUED;
        /// Don't reap, just poll status.
        const WNOWAIT = WNOWAIT;

        /// Don't wait on children of other threads in this group
        const WNOTHREAD = __WNOTHREAD;
        /// Wait on all children, regardless of type
        const WALL = __WALL;
        /// Wait for "clone" children only.
        const WCLONE = __WCLONE;
    }
}

#[derive(Debug, Clone, Copy)]
enum WaitPid {
    /// Wait for any child process
    Any,
    /// Wait for the child whose process ID is equal to the value.
    Pid(Pid),
    /// Wait for any child process whose process group ID is equal to the value.
    Pgid(Pid),
}

impl WaitPid {
    #[cfg(unittest)]
    fn apply(&self, child: &Process) -> bool {
        match self {
            WaitPid::Any => true,
            WaitPid::Pid(pid) => child.pid() == *pid,
            WaitPid::Pgid(pgid) => child.group().pgid() == *pgid,
        }
    }

    fn to_selector(self) -> wait_reap::WaitChildSelector {
        match self {
            WaitPid::Any => wait_reap::WaitChildSelector::Any,
            WaitPid::Pid(pid) => wait_reap::WaitChildSelector::Pid(pid),
            WaitPid::Pgid(pgid) => wait_reap::WaitChildSelector::Pgid(pgid),
        }
    }
}

#[cfg(unittest)]
fn wait_options_match_child(child: &Process, options: WaitOptions) -> bool {
    if options.contains(WaitOptions::WALL) {
        return true;
    }
    let is_clone_child = child.exit_signal() != Some(ksignal::Signo::SIGCHLD);
    is_clone_child == options.contains(WaitOptions::WCLONE)
}

#[cfg(unittest)]
fn matching_children(
    proc: &Arc<Process>,
    selector: WaitPid,
    options: WaitOptions,
) -> Vec<Arc<Process>> {
    proc.children()
        .into_iter()
        .filter(|child| selector.apply(child) && wait_options_match_child(child, options))
        .collect()
}

fn wait_child_kind(options: WaitOptions) -> wait_reap::WaitChildKind {
    if options.contains(WaitOptions::WALL) {
        wait_reap::WaitChildKind::Any
    } else if options.contains(WaitOptions::WCLONE) {
        wait_reap::WaitChildKind::Clone
    } else {
        wait_reap::WaitChildKind::Default
    }
}

fn wait_reap_mode(options: WaitOptions) -> wait_reap::WaitReapMode {
    if options.contains(WaitOptions::WNOWAIT) {
        wait_reap::WaitReapMode::Peek
    } else {
        wait_reap::WaitReapMode::Consume
    }
}

// `siginfo_t` is registered as a `posix_types::UserWrite` type (see
// posix-types); bit-pattern copy to userspace matches the Linux ABI.

enum WaitScanResult {
    Ready(WaitedChild),
    NoWaitableChild,
    NoMatchingChild,
}

fn scan_waitable_child(
    proc: &Arc<Process>,
    selector: WaitPid,
    options: WaitOptions,
) -> KResult<WaitScanResult> {
    match wait_reap::scan_waitable_child(
        proc,
        selector.to_selector(),
        wait_child_kind(options),
        wait_reap_mode(options),
    ) {
        wait_reap::WaitChildScan::Ready(waited) => Ok(WaitScanResult::Ready(waited)),
        wait_reap::WaitChildScan::NoMatchingChild => Ok(WaitScanResult::NoMatchingChild),
        wait_reap::WaitChildScan::NoWaitableChild => Ok(WaitScanResult::NoWaitableChild),
    }
}

unittest::path_mod_test!(wait_test_sync, "../../ut-tests/task/wait/wait_test_sync.rs");

/// Translates a caller-supplied child PID into the root number the wait
/// tables match on.
///
/// The argument is written in the caller's active PID namespace — the same
/// view `clone`/`fork` report — so a number no published process carries
/// there cannot name any child: `ECHILD`, mirroring Linux where a
/// namespace-invisible PID does not exist. Root-namespace callers skip the
/// work entirely.
fn translate_wait_pid(proc: &Arc<Process>, pid: Pid) -> KResult<Pid> {
    let observer = proc.active_pid_namespace();
    if observer.is_root() {
        return Ok(pid);
    }
    kprocess::Process::resolve_pid_in_namespace(pid, &observer)
        .ok_or(KError::from(LinuxError::ECHILD))
}

/// Blocks until a matching child reports status, then invokes `on_ready`
/// with the waited child. Shared by `wait4`/`waitpid` (writes the int exit
/// status) and `waitid` (writes a `siginfo_t`). Returns the child PID, or 0
/// under `WNOHANG` when no child is waitable.
fn wait_on_matching_children_with(
    proc: &Arc<Process>,
    selector: WaitPid,
    options: WaitOptions,
    mut on_ready: impl FnMut(&WaitedChild) -> KResult<()>,
) -> KResult<isize> {
    let current = current();

    let mut check_children = || match scan_waitable_child(proc, selector, options)? {
        WaitScanResult::Ready(waited) => {
            on_ready(&waited)?;
            // Linux reports the reaped child's number in the caller's active
            // PID namespace (pid_nr_ns), keeping the wait→kill round trip in
            // one number space; the child keeps its root number internally.
            let observer = proc.active_pid_namespace();
            let reported = waited
                .process()
                .number_in_namespace(&observer)
                .unwrap_or_else(|| waited.pid());
            Ok(Some(reported as _))
        }
        WaitScanResult::NoMatchingChild => Err(KError::from(LinuxError::ECHILD)),
        WaitScanResult::NoWaitableChild if options.contains(WaitOptions::WNOHANG) => Ok(Some(0)),
        WaitScanResult::NoWaitableChild => Ok(None),
    };

    let mut registrations = PollRegistrations::new();
    let mut interrupt_registrations = PollRegistrations::new();
    block_on(poll_fn(|cx| {
        // Check before registering so a ready zombie does not fail with ENOMEM
        // merely because waiter growth could not allocate.
        if let Some(res) = check_children().transpose() {
            return Poll::Ready(res);
        }

        let mut context = registrations.context(cx);
        if context.register(proc.child_exit_event()).is_err() {
            return Poll::Ready(Err(KError::NoMemory));
        }
        drop(context);
        #[cfg(unittest)]
        wait_test_sync::pause_after_register_if_armed();

        if let Some(res) = check_children().transpose() {
            return Poll::Ready(res);
        }

        // Match Linux wait semantics: re-scan children before honoring an
        // interrupt so a concurrent SIGCHLD cannot hide an already waitable child.
        let mut interrupt_context = interrupt_registrations.context(cx);
        match current.poll_interrupt(&mut interrupt_context) {
            Ok(Poll::Ready(())) => {
                return Poll::Ready(
                    check_children()
                        .transpose()
                        .unwrap_or_else(|| Err(KError::from(LinuxError::ERESTARTSYS))),
                );
            }
            Ok(Poll::Pending) => {}
            Err(_) => return Poll::Ready(Err(KError::NoMemory)),
        }

        Poll::Pending
    }))
}

fn wait_on_matching_children(
    proc: &Arc<Process>,
    selector: WaitPid,
    exit_code: *mut i32,
    options: WaitOptions,
) -> KResult<isize> {
    wait_on_matching_children_with(proc, selector, options, |waited| {
        if let Some(exit_code) = exit_code.check_non_null() {
            exit_code.write_vm(waited.exit_code())?;
        }
        Ok(())
    })
}

/// `waitid(2)`: wait for a child selected by `idtype`/`id` and report its
/// status through a `siginfo_t`.
///
/// Supports the full `idtype_t` set (`P_ALL`, `P_PID`, `P_PGID`, `P_PIDFD`).
/// `P_PIDFD` resolves `id` as a pidfd file descriptor and waits on its target
/// process. `WNOWAIT`/`WNOHANG`/`WEXITED` are honored by the shared wait
/// machinery; `WSTOPPED`/`WCONTINUED` are accepted for option validation but
/// stop/continue reports are not produced yet, so the wait only completes on
/// exit. The optional `rusage` argument is accepted but not populated (runc's
/// `isWaitable` probe passes `NULL`). On success returns 0 with the child
/// status in `siginfo`, matching Linux.
pub(crate) fn sys_waitid(
    idtype: u32,
    id: u32,
    siginfo: UserPtr<siginfo_t>,
    options: u32,
    _rusage: UserConstPtr<core::ffi::c_void>,
) -> KResult<isize> {
    // Linux exit.c: reject unknown bits and a request that names no report
    // class at all (`WSTOPPED` shares the `WUNTRACED` bit). Only exit reports
    // are implemented, so a request that asks solely for stop/continue is
    // rejected rather than reporting and consuming an exit it did not ask for.
    const WAITID_KNOWN: u32 = WNOHANG | WUNTRACED | WEXITED | WCONTINUED | WNOWAIT;
    if options & !WAITID_KNOWN != 0 || options & WEXITED == 0 {
        return Err(KError::from(LinuxError::EINVAL));
    }

    let current_thread = kprocess::current_user_thread();
    let proc = current_thread.process();

    // Linux `idtype_t`, matching the `P_*` constants (`u32`).
    let selector = match idtype {
        P_ALL => WaitPid::Any,
        P_PID => {
            if id == 0 {
                return Err(KError::from(LinuxError::EINVAL));
            }
            WaitPid::Pid(translate_wait_pid(proc, id)?)
        }
        P_PGID => {
            if id == 0 {
                // Linux: `P_PGID` with id 0 waits on the caller's own group.
                let current_thread = kprocess::current_user_thread();
                WaitPid::Pgid(current_thread.process().group().pgid())
            } else {
                WaitPid::Pgid(id)
            }
        }
        P_PIDFD => {
            // P_PIDFD: `id` is a pidfd file descriptor referring to a process.
            // Linux: a negative `id` is `EINVAL`, a closed or invalid descriptor
            // is `EBADF`, and a valid descriptor that is not a pidfd is `EINVAL`.
            let fd = id as i32;
            if fd < 0 {
                return Err(KError::from(LinuxError::EINVAL));
            }
            let file = kprocess::current_files().get_file(fd)?;
            let pidfd =
                kprocess::PidFd::from_file(&file).map_err(|_| KError::from(LinuxError::EINVAL))?;
            WaitPid::Pid(pidfd.process().pid())
        }
        _ => return Err(KError::from(LinuxError::EINVAL)),
    };
    let options = WaitOptions::from_bits_truncate(options);

    let mut report: Option<siginfo_t> = None;
    wait_on_matching_children_with(proc, selector, options, |waited| {
        // Linux reports the child-exit payload via a SIGCHLD-style `siginfo_t`.
        let cred = waited
            .process()
            .credentials_snapshot()
            .unwrap_or_else(|_| kcred::initial_cred());
        let (utime, stime) = waited.total_cpu_time();
        // si_pid follows the same caller-namespace projection as the wait
        // return value; Linux fills it per the waiting task's pid namespace.
        let observer = proc.active_pid_namespace();
        let reported_pid = waited
            .process()
            .number_in_namespace(&observer)
            .unwrap_or_else(|| waited.pid());
        let child = ksignal::ChildExitInfo::from_wait_status(
            reported_pid,
            cred.ruid(),
            waited.exit_code(),
            utime,
            stime,
        );
        report = Some(
            ksignal::ChildExitSignalInfo::new(ksignal::Signo::SIGCHLD, child)
                .into_signal_info()
                .0,
        );
        Ok(())
    })?;
    if !siginfo.is_null() {
        match report {
            Some(value) => siginfo.write_vm(value)?,
            None => {
                // Linux writes a zeroed report for a `WNOHANG` poll with no
                // waitable child (`si_pid = 0`); a `NULL` pointer is a no-op.
                let zeros = [0_u8; core::mem::size_of::<siginfo_t>()];
                UserPtr::<u8>::from(siginfo.as_ptr() as usize).write_vm_slice(&zeros)?;
            }
        }
    }
    Ok(0)
}

pub(crate) fn sys_waitpid(pid: i32, exit_code: *mut i32, options: u32) -> KResult<isize> {
    let options = WaitOptions::from_bits_truncate(options);

    let current_thread = kprocess::current_user_thread();
    let proc = current_thread.process();

    let selector = if pid == -1 {
        WaitPid::Any
    } else if pid == 0 {
        WaitPid::Pgid(proc.group().pgid())
    } else if pid > 0 {
        WaitPid::Pid(translate_wait_pid(proc, pid as _)?)
    } else {
        WaitPid::Pgid(-pid as _)
    };

    wait_on_matching_children(proc, selector, exit_code, options)
}

unittest::path_mod_test!(tests_wait, "../../ut-tests/task/wait/tests_wait.rs");

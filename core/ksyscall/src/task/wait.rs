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

#[unittest::mod_test]
mod wait_test_sync {
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    static REGISTER_BARRIER_ENABLED: AtomicBool = AtomicBool::new(false);
    static REGISTER_BARRIER_ARRIVALS: AtomicUsize = AtomicUsize::new(0);
    static REGISTER_BARRIER_RELEASED: AtomicBool = AtomicBool::new(false);

    pub(super) fn arm_register_barrier() {
        REGISTER_BARRIER_ARRIVALS.store(0, Ordering::Release);
        REGISTER_BARRIER_RELEASED.store(false, Ordering::Release);
        REGISTER_BARRIER_ENABLED.store(true, Ordering::Release);
    }

    pub(super) fn register_barrier_arrivals() -> usize {
        REGISTER_BARRIER_ARRIVALS.load(Ordering::Acquire)
    }

    pub(super) fn release_register_barrier() {
        REGISTER_BARRIER_RELEASED.store(true, Ordering::Release);
    }

    pub(super) fn disarm_register_barrier() {
        REGISTER_BARRIER_ENABLED.store(false, Ordering::Release);
        REGISTER_BARRIER_RELEASED.store(true, Ordering::Release);
    }

    pub(super) fn pause_after_register_if_armed() {
        if !REGISTER_BARRIER_ENABLED.load(Ordering::Acquire) {
            return;
        }

        REGISTER_BARRIER_ARRIVALS.fetch_add(1, Ordering::AcqRel);
        while !REGISTER_BARRIER_RELEASED.load(Ordering::Acquire) {
            ktask::yield_now();
        }
    }
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
            Ok(Some(waited.pid() as _))
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

    // Linux `idtype_t`, matching the `P_*` constants (`u32`).
    let selector = match idtype {
        P_ALL => WaitPid::Any,
        P_PID => {
            if id == 0 {
                return Err(KError::from(LinuxError::EINVAL));
            }
            WaitPid::Pid(id)
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

    let current_thread = kprocess::current_user_thread();
    let proc = current_thread.process();
    let mut report: Option<siginfo_t> = None;
    wait_on_matching_children_with(proc, selector, options, |waited| {
        // Linux reports the child-exit payload via a SIGCHLD-style `siginfo_t`.
        let cred = waited
            .process()
            .credentials_snapshot()
            .unwrap_or_else(|_| kcred::initial_cred());
        let (utime, stime) = waited.total_cpu_time();
        let child = ksignal::ChildExitInfo::from_wait_status(
            waited.pid(),
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
        WaitPid::Pid(pid as _)
    } else {
        WaitPid::Pgid(-pid as _)
    };

    wait_on_matching_children(proc, selector, exit_code, options)
}

#[unittest::mod_test]
mod tests_wait {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicUsize, Ordering};

    use kerrno::LinuxError;
    use kprocess::{current_user_process, process_exit, wait_reap};
    use ksignal::{SignalAction, SignalActionFlags, SignalDisposition, Signo};
    use ktask::WaitQueue;
    use unittest::{assert_eq, def_test};

    use super::{
        WaitOptions, WaitPid, matching_children, wait_on_matching_children, wait_test_sync,
    };

    fn reap_test_child(
        parent: &alloc::sync::Arc<kprocess::Process>,
        child: &alloc::sync::Arc<kprocess::Process>,
    ) {
        if !child.is_waitable_zombie() {
            process_exit::finalize_process_exit(child);
        }
        wait_reap::assert_reap_zombie_process(child);
        assert!(
            !parent
                .children()
                .iter()
                .any(|proc| proc.pid() == child.pid()),
            "test child {} should be reaped from parent",
            child.pid()
        );
    }

    fn complete_sigchld_child_exit(
        parent: &alloc::sync::Arc<kprocess::Process>,
        child: &alloc::sync::Arc<kprocess::Process>,
    ) -> bool {
        debug_assert!(
            parent
                .children()
                .iter()
                .any(|candidate| Arc::ptr_eq(candidate, child)),
            "test helper should only complete a live child exit"
        );
        process_exit::complete_process_exit(child)
    }

    #[def_test(user, serial)]
    fn test_waitable_child_rechecks_after_new_child_appears() {
        let parent = current_user_process();
        let existing = parent.fork(8_100);
        let late = parent.fork(8_101);

        let children = matching_children(&parent, WaitPid::Any, WaitOptions::empty());
        assert_eq!(children.len(), 2);
        assert!(!children.iter().any(|child| child.is_waitable_zombie()));

        process_exit::finalize_process_exit(&late);

        let children = matching_children(&parent, WaitPid::Any, WaitOptions::empty());
        let waitable = children
            .iter()
            .find(|child| child.is_waitable_zombie())
            .cloned()
            .expect("wait must observe child that exits after initial scan");
        assert_eq!(waitable.pid(), late.pid());

        reap_test_child(&parent, &late);
        reap_test_child(&parent, &existing);
    }

    #[def_test(user, serial)]
    fn test_matching_children_reflects_reaped_children() {
        let parent = current_user_process();
        let child = parent.fork(8_110);

        assert_eq!(
            matching_children(&parent, WaitPid::Pid(child.pid()), WaitOptions::empty()).len(),
            1
        );

        reap_test_child(&parent, &child);

        assert!(
            matching_children(&parent, WaitPid::Pid(child.pid()), WaitOptions::empty()).is_empty(),
            "matching child set must be recomputed from the live parent/child relation"
        );
    }

    #[def_test(user, serial)]
    fn test_wait_options_filter_clone_children_by_exit_signal() {
        let parent = current_user_process();
        let sigchld_child = parent.fork(8_115);
        let clone_child = parent.fork_with_exit_signal(8_116, Some(Signo::SIGUSR1));

        let default_children = matching_children(&parent, WaitPid::Any, WaitOptions::empty());
        assert!(
            default_children
                .iter()
                .any(|child| child.pid() == sigchld_child.pid())
        );
        assert!(
            default_children
                .iter()
                .all(|child| child.pid() != clone_child.pid())
        );

        let clone_children = matching_children(&parent, WaitPid::Any, WaitOptions::WCLONE);
        assert!(
            clone_children
                .iter()
                .any(|child| child.pid() == clone_child.pid())
        );
        assert!(
            clone_children
                .iter()
                .all(|child| child.pid() != sigchld_child.pid())
        );

        let all_children = matching_children(&parent, WaitPid::Any, WaitOptions::WALL);
        assert!(
            all_children
                .iter()
                .any(|child| child.pid() == sigchld_child.pid())
        );
        assert!(
            all_children
                .iter()
                .any(|child| child.pid() == clone_child.pid())
        );

        reap_test_child(&parent, &sigchld_child);
        reap_test_child(&parent, &clone_child);
    }

    #[def_test(user, serial)]
    fn test_waitpid_prefers_ready_zombie_over_pending_interrupt() {
        let parent = current_user_process();
        let child = parent.fork(8_120);

        process_exit::finalize_process_exit(&child);
        ktask::current().interrupt();

        let waited = wait_on_matching_children(
            &parent,
            WaitPid::Pid(child.pid()),
            core::ptr::null_mut(),
            WaitOptions::empty(),
        )
        .expect("wait must reap an already exited child before returning ERESTARTSYS");

        ktask::current().clear_interrupt();
        assert_eq!(waited, child.pid() as isize);
        assert!(
            matching_children(&parent, WaitPid::Pid(child.pid()), WaitOptions::empty()).is_empty(),
            "reaped child must be removed from the live child set"
        );
    }

    #[def_test(user, serial)]
    fn test_wnowait_reports_without_reaping_child() {
        let parent = current_user_process();
        let child = parent.fork(8_119);

        process_exit::finalize_process_exit(&child);

        let peeked = wait_on_matching_children(
            &parent,
            WaitPid::Pid(child.pid()),
            core::ptr::null_mut(),
            WaitOptions::WNOWAIT,
        )
        .expect("WNOWAIT must report a waitable zombie");

        assert_eq!(peeked, child.pid() as isize);
        assert!(
            matching_children(&parent, WaitPid::Pid(child.pid()), WaitOptions::empty())
                .iter()
                .any(|candidate| candidate.is_waitable_zombie()),
            "WNOWAIT must leave the child waitable"
        );

        let reaped = wait_on_matching_children(
            &parent,
            WaitPid::Pid(child.pid()),
            core::ptr::null_mut(),
            WaitOptions::empty(),
        )
        .expect("a later wait must still be able to reap the child");

        assert_eq!(reaped, child.pid() as isize);
        assert!(
            matching_children(&parent, WaitPid::Pid(child.pid()), WaitOptions::empty()).is_empty(),
            "ordinary wait must consume the child after WNOWAIT"
        );
    }

    #[def_test(user, serial)]
    fn test_waitpid_reports_restartsys_when_only_interrupt_is_ready() {
        let parent = current_user_process();
        let child = parent.fork(8_121);

        ktask::current().interrupt();
        let err = wait_on_matching_children(
            &parent,
            WaitPid::Pid(child.pid()),
            core::ptr::null_mut(),
            WaitOptions::empty(),
        )
        .expect_err("wait without a waitable child should still report interruption");
        ktask::current().clear_interrupt();

        assert_eq!(LinuxError::from(err), LinuxError::ERESTARTSYS);
        reap_test_child(&parent, &child);
    }

    #[def_test(user, serial)]
    fn test_concurrent_waiters_must_rescan_after_register_before_reaping() {
        let parent = current_user_process();
        let child = parent.fork(8_122);
        let child_pid = child.pid();

        let reaped = Arc::new(AtomicUsize::new(0));
        let echld = Arc::new(AtomicUsize::new(0));
        let unexpected = Arc::new(AtomicUsize::new(0));
        let finished = Arc::new(AtomicUsize::new(0));
        let finished_wq = Arc::new(WaitQueue::new());

        wait_test_sync::arm_register_barrier();

        let spawn_waiter = || {
            let parent = parent.clone();
            let reaped = reaped.clone();
            let echld = echld.clone();
            let unexpected = unexpected.clone();
            let finished = finished.clone();
            let finished_wq = finished_wq.clone();
            ktask::spawn(move || {
                match wait_on_matching_children(
                    &parent,
                    WaitPid::Pid(child_pid),
                    core::ptr::null_mut(),
                    WaitOptions::empty(),
                )
                .map_err(LinuxError::from)
                {
                    Ok(pid) if pid == child_pid as isize => {
                        reaped.fetch_add(1, Ordering::AcqRel);
                    }
                    Err(LinuxError::ECHILD) => {
                        echld.fetch_add(1, Ordering::AcqRel);
                    }
                    _ => {
                        unexpected.fetch_add(1, Ordering::AcqRel);
                    }
                }
                finished.fetch_add(1, Ordering::AcqRel);
                finished_wq.notify_one(true);
            })
        };

        let waiter_a = spawn_waiter();
        let waiter_b = spawn_waiter();

        // Wait path checks for a ready child before registering. Keep the child
        // live until both waiters have registered, otherwise they return early
        // and never hit the barrier (hanging this test).
        while wait_test_sync::register_barrier_arrivals() < 2 {
            ktask::yield_now();
        }
        process_exit::finalize_process_exit(&child);
        wait_test_sync::release_register_barrier();

        finished_wq.wait_until(|| finished.load(Ordering::Acquire) == 2);
        wait_test_sync::disarm_register_barrier();

        assert_eq!(waiter_a.join(), 0);
        assert_eq!(waiter_b.join(), 0);

        assert_eq!(
            reaped.load(Ordering::Acquire),
            1,
            "exactly one waiter must win the reap race"
        );
        assert_eq!(
            echld.load(Ordering::Acquire),
            1,
            "the losing waiter must re-scan the live child set instead of reusing a stale snapshot"
        );
        assert_eq!(
            unexpected.load(Ordering::Acquire),
            0,
            "both waiters should resolve to either the reaped pid or ECHILD"
        );

        assert!(
            matching_children(&parent, WaitPid::Pid(child_pid), WaitOptions::empty()).is_empty(),
            "the child must be removed from the live child set after exactly one waiter reaps it"
        );
    }

    #[def_test(user, serial)]
    fn test_sigchld_explicit_ignore_autoreap_wakes_waiter_with_echild() {
        let parent = current_user_process();
        let child = parent.fork(8_230);
        let child_pid = child.pid();
        let signals = parent
            .signal_manager()
            .expect("current process signal manager");
        let old_action = signals
            .replace_signal_action(
                Signo::SIGCHLD,
                Some(SignalAction {
                    disposition: SignalDisposition::Ignore,
                    ..Default::default()
                }),
            )
            .unwrap();

        let echld = Arc::new(AtomicUsize::new(0));
        let reaped = Arc::new(AtomicUsize::new(0));
        let unexpected = Arc::new(AtomicUsize::new(0));
        let finished = Arc::new(AtomicUsize::new(0));
        let finished_wq = Arc::new(WaitQueue::new());

        wait_test_sync::arm_register_barrier();

        let waiter = {
            let parent = parent.clone();
            let echld = echld.clone();
            let reaped = reaped.clone();
            let unexpected = unexpected.clone();
            let finished = finished.clone();
            let finished_wq = finished_wq.clone();
            ktask::spawn(move || {
                match wait_on_matching_children(
                    &parent,
                    WaitPid::Pid(child_pid),
                    core::ptr::null_mut(),
                    WaitOptions::empty(),
                )
                .map_err(LinuxError::from)
                {
                    Err(LinuxError::ECHILD) => {
                        echld.fetch_add(1, Ordering::AcqRel);
                    }
                    Ok(pid) if pid == child_pid as isize => {
                        reaped.fetch_add(1, Ordering::AcqRel);
                    }
                    _ => {
                        unexpected.fetch_add(1, Ordering::AcqRel);
                    }
                }
                finished.fetch_add(1, Ordering::AcqRel);
                finished_wq.notify_one(true);
            })
        };

        while wait_test_sync::register_barrier_arrivals() < 1 {
            ktask::yield_now();
        }
        assert!(
            complete_sigchld_child_exit(&parent, &child),
            "explicit SIG_IGN must request autoreap"
        );
        wait_test_sync::release_register_barrier();

        finished_wq.wait_until(|| finished.load(Ordering::Acquire) == 1);
        wait_test_sync::disarm_register_barrier();
        assert_eq!(waiter.join(), 0);
        signals
            .set_signal_action(Signo::SIGCHLD, old_action)
            .unwrap();

        assert_eq!(echld.load(Ordering::Acquire), 1);
        assert_eq!(
            reaped.load(Ordering::Acquire),
            0,
            "autoreaped child must not be returned from wait"
        );
        assert_eq!(unexpected.load(Ordering::Acquire), 0);
        assert!(
            matching_children(&parent, WaitPid::Pid(child_pid), WaitOptions::empty()).is_empty(),
            "autoreap must remove the child before parent waiters resume"
        );
    }

    #[def_test(user, serial)]
    fn test_sigchld_nocldwait_autoreap_leaves_no_waitable_child() {
        let parent = current_user_process();
        let child = parent.fork(8_231);
        let child_pid = child.pid();
        let signals = parent
            .signal_manager()
            .expect("current process signal manager");
        let old_action = signals
            .replace_signal_action(
                Signo::SIGCHLD,
                Some(SignalAction {
                    flags: SignalActionFlags::NOCLDWAIT,
                    ..Default::default()
                }),
            )
            .unwrap();

        assert!(
            complete_sigchld_child_exit(&parent, &child),
            "SA_NOCLDWAIT must request autoreap"
        );

        let err = wait_on_matching_children(
            &parent,
            WaitPid::Pid(child_pid),
            core::ptr::null_mut(),
            WaitOptions::WNOHANG,
        )
        .expect_err("autoreaped child must not remain waitable");
        signals
            .set_signal_action(Signo::SIGCHLD, old_action)
            .unwrap();

        assert_eq!(LinuxError::from(err), LinuxError::ECHILD);
    }
}

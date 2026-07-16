// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process waiting and status syscalls.
//!
//! This module implements process status waiting operations including:
//! - Wait for process termination (wait, waitpid, waitid, etc.)
//! - Process status retrieval and interpretation
//! - Child process status monitoring

use alloc::{sync::Arc, vec::Vec};
use core::{future::poll_fn, task::Poll};

use bitflags::bitflags;
use kerrno::{KError, KResult, LinuxError};
use kprocess::{Pid, Process, wait_reap};
use ktask::{current, future::block_on};
use linux_raw_sys::general::{
    __WALL, __WCLONE, __WNOTHREAD, WCONTINUED, WEXITED, WNOHANG, WNOWAIT, WUNTRACED,
};
use osvm::{VirtMutPtr, VirtPtr};

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
    fn apply(&self, child: &Process) -> bool {
        match self {
            WaitPid::Any => true,
            WaitPid::Pid(pid) => child.pid() == *pid,
            WaitPid::Pgid(pgid) => child.group().pgid() == *pgid,
        }
    }
}

fn matching_children(proc: &Arc<Process>, selector: WaitPid) -> Vec<Arc<Process>> {
    proc.children()
        .into_iter()
        .filter(|child| selector.apply(child))
        .collect()
}

fn waitable_zombie_child(children: &[Arc<Process>]) -> Option<Arc<Process>> {
    children.iter().find(|child| child.is_zombie()).cloned()
}

fn reap_waitable_zombie_child(
    proc: &Arc<Process>,
    children: &[Arc<Process>],
    exit_code: *mut i32,
    options: WaitOptions,
) -> KResult<Option<isize>> {
    let mut saw_zombie = false;

    for child in children {
        if !child.is_zombie() {
            continue;
        }

        saw_zombie = true;
        let child_exit_code = child.exit_code();
        if options.contains(WaitOptions::WNOWAIT) {
            if let Some(exit_code) = exit_code.check_non_null() {
                exit_code.write_vm(child_exit_code)?;
            }
            return Ok(Some(child.pid() as _));
        }

        if !wait_reap::try_reap_zombie_process(child) {
            continue;
        }

        // Accumulate reaped child's CPU time into the parent after winning the
        // single-reaper race against concurrent waiters.
        let (thread_utime_ns, thread_stime_ns) = child.exited_thread_time_ns();
        let (child_utime_ns, child_stime_ns) = child.child_time_ns();
        let utime_ns = thread_utime_ns.saturating_add(child_utime_ns);
        let stime_ns = thread_stime_ns.saturating_add(child_stime_ns);
        wait_reap::record_reaped_child_cpu_time(proc, utime_ns, stime_ns);

        if let Some(exit_code) = exit_code.check_non_null() {
            exit_code.write_vm(child_exit_code)?;
        }
        return Ok(Some(child.pid() as _));
    }

    if saw_zombie {
        // A concurrent waiter may have already consumed the zombie selected by
        // this snapshot. Force the caller to re-scan the current live child set
        // instead of acting on a stale view.
        Ok(None)
    } else {
        Ok(None)
    }
}

#[cfg(unittest)]
mod wait_test_sync {
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    static REGISTER_BARRIER_ENABLED: AtomicBool = AtomicBool::new(false);
    static REGISTER_BARRIER_ARRIVALS: AtomicUsize = AtomicUsize::new(0);
    static REGISTER_BARRIER_RELEASED: AtomicBool = AtomicBool::new(false);

    pub fn arm_register_barrier() {
        REGISTER_BARRIER_ARRIVALS.store(0, Ordering::Release);
        REGISTER_BARRIER_RELEASED.store(false, Ordering::Release);
        REGISTER_BARRIER_ENABLED.store(true, Ordering::Release);
    }

    pub fn register_barrier_arrivals() -> usize {
        REGISTER_BARRIER_ARRIVALS.load(Ordering::Acquire)
    }

    pub fn release_register_barrier() {
        REGISTER_BARRIER_RELEASED.store(true, Ordering::Release);
    }

    pub fn disarm_register_barrier() {
        REGISTER_BARRIER_ENABLED.store(false, Ordering::Release);
        REGISTER_BARRIER_RELEASED.store(true, Ordering::Release);
    }

    pub fn pause_after_register_if_armed() {
        if !REGISTER_BARRIER_ENABLED.load(Ordering::Acquire) {
            return;
        }

        REGISTER_BARRIER_ARRIVALS.fetch_add(1, Ordering::AcqRel);
        while !REGISTER_BARRIER_RELEASED.load(Ordering::Acquire) {
            ktask::yield_now();
        }
    }
}

fn wait_on_matching_children(
    proc: &Arc<Process>,
    selector: WaitPid,
    exit_code: *mut i32,
    options: WaitOptions,
) -> KResult<isize> {
    // FIXME: add back support for WALL & WCLONE. The stable child-selection path
    // now hangs off `Process`, but clone-child classification still needs an
    // explicit process-level model instead of depending on transient runtime state.
    let current = current();

    let check_children = || {
        loop {
            let children = matching_children(proc, selector);
            if children.is_empty() {
                return Err(KError::from(LinuxError::ECHILD));
            }

            if let Some(result) = reap_waitable_zombie_child(proc, &children, exit_code, options)? {
                return Ok(Some(result));
            }

            if waitable_zombie_child(&children).is_some() {
                continue;
            }

            if options.contains(WaitOptions::WNOHANG) {
                return Ok(Some(0));
            } else {
                return Ok(None);
            }
        }
    };

    block_on(poll_fn(|cx| {
        proc.child_exit_event().register(cx.waker());
        #[cfg(unittest)]
        wait_test_sync::pause_after_register_if_armed();

        if let Some(res) = check_children().transpose() {
            return Poll::Ready(res);
        }

        // Match Linux wait semantics: re-scan children before honoring an
        // interrupt so a concurrent SIGCHLD cannot hide an already waitable child.
        if current.poll_interrupt(cx).is_ready() {
            return Poll::Ready(
                check_children()
                    .transpose()
                    .unwrap_or_else(|| Err(KError::from(LinuxError::ERESTARTSYS))),
            );
        }

        Poll::Pending
    }))
}

pub fn sys_waitpid(pid: i32, exit_code: *mut i32, options: u32) -> KResult<isize> {
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

#[cfg(unittest)]
mod tests_wait {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicUsize, Ordering};

    use kerrno::LinuxError;
    use kprocess::{current_user_process, process_exit, wait_reap};
    use ktask::WaitQueue;
    use unittest::{assert_eq, def_test};

    use super::{
        WaitOptions, WaitPid, matching_children, wait_on_matching_children, wait_test_sync,
        waitable_zombie_child,
    };

    fn reap_test_child(
        parent: &alloc::sync::Arc<kprocess::Process>,
        child: &alloc::sync::Arc<kprocess::Process>,
    ) {
        if !child.is_zombie() {
            process_exit::finalize_process_exit(child);
        }
        wait_reap::reap_zombie_process(child);
        assert!(
            !parent
                .children()
                .iter()
                .any(|proc| proc.pid() == child.pid()),
            "test child {} should be reaped from parent",
            child.pid()
        );
    }

    #[def_test(user, serial)]
    fn test_waitable_child_rechecks_after_new_child_appears() {
        let parent = current_user_process();
        let existing = parent.fork(8_100);
        let late = parent.fork(8_101);

        let children = matching_children(&parent, WaitPid::Any);
        assert_eq!(children.len(), 2);
        assert!(waitable_zombie_child(&children).is_none());

        process_exit::finalize_process_exit(&late);

        let children = matching_children(&parent, WaitPid::Any);
        let waitable = waitable_zombie_child(&children)
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
            matching_children(&parent, WaitPid::Pid(child.pid())).len(),
            1
        );

        reap_test_child(&parent, &child);

        assert!(
            matching_children(&parent, WaitPid::Pid(child.pid())).is_empty(),
            "matching child set must be recomputed from the live parent/child relation"
        );
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
            matching_children(&parent, WaitPid::Pid(child.pid())).is_empty(),
            "reaped child must be removed from the live child set"
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
        process_exit::finalize_process_exit(&child);

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

        while wait_test_sync::register_barrier_arrivals() < 2 {
            ktask::yield_now();
        }
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
            matching_children(&parent, WaitPid::Pid(child_pid)).is_empty(),
            "the child must be removed from the live child set after exactly one waiter reaps it"
        );
    }
}

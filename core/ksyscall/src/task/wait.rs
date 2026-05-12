// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process waiting and status syscalls.
//!
//! This module implements process status waiting operations including:
//! - Wait for process termination (wait, waitpid, waitid, etc.)
//! - Process status retrieval and interpretation
//! - Child process status monitoring

use alloc::vec::Vec;
use core::{future::poll_fn, task::Poll};

use bitflags::bitflags;
use kerrno::{KError, KResult, LinuxError};
use kprocess::{Pid, Process};
use ktask::future::{block_on, interruptible};
use kthread::{AsThread, get_process_state, get_task};
use linux_raw_sys::general::{
    __WALL, __WCLONE, __WNOTHREAD, WCONTINUED, WEXITED, WNOHANG, WNOWAIT, WUNTRACED,
};
use osvm::{VirtMutPtr, VirtPtr};

bitflags! {
    #[derive(Debug)]
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

pub fn sys_waitpid(pid: i32, exit_code: *mut i32, options: u32) -> KResult<isize> {
    let options = WaitOptions::from_bits_truncate(options);
    info!("sys_waitpid <= pid: {pid:?}, options: {options:?}");

    let current_thread = kthread::current_thread();
    let proc_state = current_thread.process_state();
    let proc = &proc_state.proc;

    let pid = if pid == -1 {
        WaitPid::Any
    } else if pid == 0 {
        WaitPid::Pgid(proc.group().pgid())
    } else if pid > 0 {
        WaitPid::Pid(pid as _)
    } else {
        WaitPid::Pgid(-pid as _)
    };

    // FIXME: add back support for WALL & WCLONE, since ProcessState may drop before
    // Process now.
    let children = proc
        .children()
        .into_iter()
        .filter(|child| pid.apply(child))
        .collect::<Vec<_>>();
    if children.is_empty() {
        return Err(KError::from(LinuxError::ECHILD));
    }

    let check_children = || {
        if let Some(child) = children.iter().find(|child| child.is_zombie()) {
            if !options.contains(WaitOptions::WNOWAIT) {
                // Accumulate reaped child's CPU time into the parent.
                if let Ok(child_proc_state) = get_process_state(child.pid()) {
                    let (mut utime_ns, mut stime_ns) = child_proc_state.child_time_ns();
                    for tid in child.threads() {
                        if let Ok(task) = get_task(tid) {
                            let thr = task.as_thread();
                            let (u, s) = thr.time.lock().output();
                            utime_ns +=
                                u.as_secs() as usize * 1_000_000_000 + u.subsec_nanos() as usize;
                            stime_ns +=
                                s.as_secs() as usize * 1_000_000_000 + s.subsec_nanos() as usize;
                        }
                    }
                    proc_state.accumulate_child_time(utime_ns, stime_ns);
                }
                child.free();
            }
            if let Some(exit_code) = exit_code.check_non_null() {
                exit_code.write_vm(child.exit_code())?;
            }
            Ok(Some(child.pid() as _))
        } else if options.contains(WaitOptions::WNOHANG) {
            Ok(Some(0))
        } else {
            Ok(None)
        }
    };

    block_on(interruptible(poll_fn(|cx| {
        match check_children().transpose() {
            Some(res) => Poll::Ready(res),
            None => {
                proc_state.child_exit_event().register(cx.waker());
                Poll::Pending
            }
        }
    })))?
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Signal handling syscalls.
//!
//! This module implements signal-related system calls including:
//! - Signal mask manipulation (rt_sigprocmask, rt_sigaction, etc.)
//! - Signal sending (kill, tgkill, sigqueue, etc.)
//! - Signal waiting (pause, rt_sigsuspend, etc.)
//! - Alternate signal stacks (sigaltstack)
//! - Real-time signal operations
use core::{future::poll_fn, task::Poll};

use kcore::task::{
    AsThread, processes, send_signal_to_process, send_signal_to_process_group,
    send_signal_to_thread,
};
use kerrno::{KError, KResult, LinuxError};
use khal::uspace::UserContext;
use kprocess::Pid;
use ksignal::{SignalInfo, SignalSet, SignalStack, Signo};
use ktask::{
    current,
    future::{self, block_on},
};
use linux_raw_sys::general::{
    MINSIGSTKSZ, SI_TKILL, SI_USER, SIG_BLOCK, SIG_SETMASK, SIG_UNBLOCK, kernel_sigaction, siginfo,
    timespec,
};
use osvm::{VirtMutPtr, VirtPtr};

use crate::{
    signal::{block_next_signal, check_signals},
    time::TimeValueLike,
};

/// Validates that the signal set size matches the expected size.
pub(crate) fn check_sigset_size(size: usize) -> KResult<()> {
    if size != size_of::<SignalSet>() && size != 0 {
        return Err(KError::InvalidInput);
    }
    Ok(())
}

/// Converts a numeric signal number to Signo enum.
fn parse_signo(signo: u32) -> KResult<Signo> {
    Signo::from_repr(signo as u8).ok_or(KError::InvalidInput)
}

/// Manages the signal mask for the current thread.
/// Allows blocking/unblocking signals or replacing the entire mask.
/// Manipulate the signal mask for the current thread
/// Allows blocking, unblocking, or replacing the entire signal mask
pub fn sys_rt_sigprocmask(
    how: i32,
    set: *const SignalSet,
    oldset: *mut SignalSet,
    sigsetsize: usize,
) -> KResult<isize> {
    check_sigset_size(sigsetsize)?;

    let curr = current();
    let sig = &curr.as_thread().signal;
    // Get the current signal mask
    let old = sig.blocked();

    // If oldset is provided, return the old mask to user space
    if let Some(oldset) = oldset.check_non_null() {
        oldset.write_vm(old)?;
    }

    // If a new mask is provided, apply the requested operation
    if let Some(set) = set.check_non_null() {
        let set = unsafe { set.read_uninit()?.assume_init() };

        // Apply the mask operation based on 'how' parameter
        let set = match how as u32 {
            SIG_BLOCK => old | set,    // Add signals to the mask
            SIG_UNBLOCK => old & !set, // Remove signals from the mask
            SIG_SETMASK => set,        // Replace the entire mask
            _ => return Err(KError::InvalidInput),
        };

        debug!("sys_rt_sigprocmask <= {set:?}");
        sig.set_blocked(set);
    }

    Ok(0)
}

/// Set or retrieve the action for a signal
pub fn sys_rt_sigaction(
    signo: u32,
    act: *const kernel_sigaction,
    oldact: *mut kernel_sigaction,
    sigsetsize: usize,
) -> KResult<isize> {
    check_sigset_size(sigsetsize)?;

    let signo = parse_signo(signo)?;
    if matches!(signo, Signo::SIGKILL | Signo::SIGSTOP) {
        return Err(KError::InvalidInput);
    }

    let curr = current();
    let mut actions = curr.as_thread().proc_data.signal.actions.lock();
    if let Some(oldact) = oldact.check_non_null() {
        oldact.write_vm(actions[signo].clone().into())?;
    }
    if let Some(act) = act.check_non_null() {
        let act = unsafe { act.read_uninit()?.assume_init() }.into();
        debug!("sys_rt_sigaction <= signo: {signo:?}, act: {act:?}");
        actions[signo] = act;
    }
    Ok(0)
}

/// Get the set of pending signals
pub fn sys_rt_sigpending(set: *mut SignalSet, sigsetsize: usize) -> KResult<isize> {
    check_sigset_size(sigsetsize)?;
    set.write_vm(current().as_thread().signal.pending())?;
    Ok(0)
}

fn make_siginfo(signo: u32, code: i32) -> KResult<Option<SignalInfo>> {
    if signo == 0 {
        return Ok(None);
    }
    let signo = parse_signo(signo)?;
    Ok(Some(SignalInfo::new_user(
        signo,
        code,
        current().as_thread().proc_data.proc.pid(),
    )))
}

/// Send a signal to a process or process group
pub fn sys_kill(pid: i32, signo: u32) -> KResult<isize> {
    debug!("sys_kill: pid = {pid}, signo = {signo}");
    let sig = make_siginfo(signo, SI_USER as _)?;

    match pid {
        1.. => {
            send_signal_to_process(pid as _, sig)?;
        }
        0 => {
            let pgid = current().as_thread().proc_data.proc.group().pgid();
            send_signal_to_process_group(pgid, sig)?;
        }
        -1 => {
            let curr_pid = current().as_thread().proc_data.proc.pid();
            if let Some(sig) = sig {
                for proc_data in processes() {
                    // POSIX.1 requires that kill(-1,sig) send sig to all processes that
                    //    the calling process may send signals to, except possibly for some
                    //    implementation-defined system processes.  Linux allows a process
                    //    to signal itself, but on Linux the call kill(-1,sig) does not
                    //    signal the calling process.
                    if proc_data.proc.is_init() || proc_data.proc.pid() == curr_pid {
                        continue;
                    }
                    let _ = send_signal_to_process(proc_data.proc.pid(), Some(sig.clone()));
                }
            }
        }
        ..-1 => {
            send_signal_to_process_group((-pid) as Pid, sig)?;
        }
    }
    Ok(0)
}

/// Send a signal to a specific thread
pub fn sys_tkill(tid: Pid, signo: u32) -> KResult<isize> {
    let sig = make_siginfo(signo, SI_TKILL)?;
    send_signal_to_thread(None, tid, sig)?;
    Ok(0)
}

/// Send a signal to a thread within a specific thread group
pub fn sys_tgkill(tgid: Pid, tid: Pid, signo: u32) -> KResult<isize> {
    let sig = make_siginfo(signo, SI_TKILL)?;
    send_signal_to_thread(Some(tgid), tid, sig)?;
    Ok(0)
}

pub(crate) fn make_queue_signal_info(
    tgid: Pid,
    signo: u32,
    sig: *const SignalInfo,
) -> KResult<Option<SignalInfo>> {
    if signo == 0 {
        return Ok(None);
    }

    let signo = parse_signo(signo)?;
    let mut sig = unsafe { sig.read_uninit()?.assume_init() };
    sig.set_signo(signo);
    if current().as_thread().proc_data.proc.pid() != tgid
        && (sig.code() >= 0 || sig.code() == SI_TKILL)
    {
        return Err(KError::OperationNotPermitted);
    }
    Ok(Some(sig))
}

/// Queue a real-time signal with additional information to a process
pub fn sys_rt_sigqueueinfo(
    tgid: Pid,
    signo: u32,
    sig: *const SignalInfo,
    sigsetsize: usize,
) -> KResult<isize> {
    check_sigset_size(sigsetsize)?;

    let sig = make_queue_signal_info(tgid, signo, sig)?;
    send_signal_to_process(tgid, sig)?;
    Ok(0)
}

/// Queue a real-time signal with additional information to a specific thread
pub fn sys_rt_tgsigqueueinfo(
    tgid: Pid,
    tid: Pid,
    signo: u32,
    sig: *const SignalInfo,
    sigsetsize: usize,
) -> KResult<isize> {
    check_sigset_size(sigsetsize)?;

    let sig = make_queue_signal_info(tgid, signo, sig)?;
    send_signal_to_thread(Some(tgid), tid, sig)?;
    Ok(0)
}

/// Return from signal handler and restore context
pub fn sys_rt_sigreturn(uctx: &mut UserContext) -> KResult<isize> {
    block_next_signal();
    current().as_thread().signal.restore(uctx);
    Ok(uctx.retval() as isize)
}

/// Wait for a signal from a specified set with optional timeout
pub fn sys_rt_sigtimedwait(
    uctx: &mut UserContext,
    set: *const SignalSet,
    info: *mut siginfo,
    timeout: *const timespec,
    sigsetsize: usize,
) -> KResult<isize> {
    check_sigset_size(sigsetsize)?;

    let set = unsafe { set.read_uninit()?.assume_init() };

    let timeout = if let Some(ts) = timeout.check_non_null() {
        let ts = unsafe { ts.read_uninit()?.assume_init() };
        Some(ts.try_into_time_value()?)
    } else {
        None
    };

    debug!("sys_rt_sigtimedwait => set = {set:?}, timeout = {timeout:?}");

    let curr = current();
    let thr = curr.as_thread();
    let signal = &thr.signal;

    let old_blocked = signal.blocked();
    signal.set_blocked(old_blocked & !set);

    uctx.set_retval(-LinuxError::EINTR.into_raw() as usize);
    let fut = poll_fn(|cx| {
        if let Some(sig) = signal.dequeue_signal(&set) {
            signal.set_blocked(old_blocked);
            Poll::Ready(Some(sig))
        } else if check_signals(thr, uctx, Some(old_blocked)) {
            Poll::Ready(None)
        } else {
            let _ = curr.poll_interrupt(cx);
            Poll::Pending
        }
    });

    let Ok(sig) = block_on(future::timeout(timeout, fut)) else {
        // Timeout
        signal.set_blocked(old_blocked);
        return Err(KError::WouldBlock);
    };
    let Some(sig) = sig else {
        // Interrupted
        return Ok(0);
    };

    if let Some(info) = info.check_non_null() {
        info.write_vm(sig.0)?;
    }

    Ok(sig.signo() as _)
}

/// Replace signal mask and suspend execution until a signal is delivered
pub fn sys_rt_sigsuspend(
    uctx: &mut UserContext,
    set: *const SignalSet,
    sigsetsize: usize,
) -> KResult<isize> {
    check_sigset_size(sigsetsize)?;

    let curr = current();
    let thr = curr.as_thread();

    let set = unsafe { set.read_uninit()?.assume_init() };
    let old_blocked = thr.signal.set_blocked(set);

    // sigsuspend always returns -EINTR when a signal is caught
    // We set this in uctx before check_signals so it's saved in SignalFrame
    uctx.set_retval(-LinuxError::EINTR.into_raw() as usize);

    block_on(poll_fn(|cx| {
        if check_signals(thr, uctx, Some(old_blocked)) {
            return Poll::Ready(());
        }
        let _ = curr.poll_interrupt(cx);
        Poll::Pending
    }));

    // sigsuspend always returns -EINTR
    Err(KError::Interrupted)
}

/// Set or retrieve the alternate signal stack
pub fn sys_sigaltstack(ss: *const SignalStack, old_ss: *mut SignalStack) -> KResult<isize> {
    let curr = current();
    let sig = &curr.as_thread().signal;

    if let Some(old_ss) = old_ss.check_non_null() {
        old_ss.write_vm(sig.stack())?;
    }

    if let Some(ss) = ss.check_non_null() {
        let ss = unsafe { ss.read_uninit()?.assume_init() };
        if ss.size <= MINSIGSTKSZ as usize {
            return Err(KError::NoMemory);
        }
        sig.set_stack(ss);
    }
    Ok(0)
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! POSIX signal syscall implementations.
//!
//! - Signal mask manipulation (`rt_sigprocmask`, `rt_sigaction`, `rt_sigpending`)
//! - Signal delivery (`kill`, `tkill`, `tgkill`, realtime queueing)
//! - Interval timers (`getitimer`, `setitimer`)
//! - Signal wait/return flow (`rt_sigreturn`, `rt_sigtimedwait`, `rt_sigsuspend`)
//! - Alternate signal stack (`sigaltstack`)

#![no_std]

mod itimer;

#[macro_use]
extern crate klogger;

use core::{future::poll_fn, task::Poll};

pub use itimer::{sys_getitimer, sys_setitimer};
use kerrno::{KError, KResult, LinuxError};
use khal::uspace::UserContext;
use kprocess::Pid;
use kservices::signal::{block_next_signal, check_signals};
use ksignal::{SignalInfo, SignalSet, SignalStack, Signo};
use ktask::{
    current,
    future::{self, block_on},
};
use kthread::{
    processes, send_signal_to_process, send_signal_to_process_group, send_signal_to_thread,
};
use linux_raw_sys::general::{
    MINSIGSTKSZ, SI_TKILL, SI_USER, SIG_BLOCK, SIG_SETMASK, SIG_UNBLOCK, timespec,
};
use posix_types::{
    TimeValueLike, UserConstPtr, UserPtr, k_sigaction, k_sigaltstack, k_siginfo, k_sigset,
};

/// Validates that the signal set size matches the expected size.
pub fn check_sigset_size(size: usize) -> KResult<()> {
    if size != size_of::<k_sigset>() && size != 0 {
        return Err(KError::InvalidInput);
    }
    Ok(())
}

/// Converts a numeric signal number to [`Signo`].
fn parse_signo(signo: u32) -> KResult<Signo> {
    Signo::from_repr(signo as u8).ok_or(KError::InvalidInput)
}

/// Manipulates the signal mask for the current thread.
///
/// See <https://man7.org/linux/man-pages/man2/rt_sigprocmask.2.html>.
pub fn sys_rt_sigprocmask(
    how: i32,
    set: UserConstPtr<k_sigset>,
    oldset: UserPtr<k_sigset>,
    sigsetsize: usize,
) -> KResult<isize> {
    check_sigset_size(sigsetsize)?;

    let signal = &kthread::current_thread().signal;
    let old = signal.blocked();

    if let Some(oldset) = oldset.check_non_null() {
        oldset.write_vm(old.into())?;
    }

    if let Some(set) = set.check_non_null() {
        let set: SignalSet = set.read_vm()?.into();
        let set = match how as u32 {
            SIG_BLOCK => old | set,
            SIG_UNBLOCK => old & !set,
            SIG_SETMASK => set,
            _ => return Err(KError::InvalidInput),
        };

        debug!("sys_rt_sigprocmask <= {set:?}");
        signal.set_blocked(set);
    }

    Ok(0)
}

/// Sets or retrieves the action for a signal.
///
/// See <https://man7.org/linux/man-pages/man2/rt_sigaction.2.html>.
pub fn sys_rt_sigaction(
    signo: u32,
    act: UserConstPtr<k_sigaction>,
    oldact: UserPtr<k_sigaction>,
    sigsetsize: usize,
) -> KResult<isize> {
    check_sigset_size(sigsetsize)?;

    let signo = parse_signo(signo)?;
    if matches!(signo, Signo::SIGKILL | Signo::SIGSTOP) {
        return Err(KError::InvalidInput);
    }

    let current_thread = kthread::current_thread();
    let mut actions = current_thread.process_state().signal.actions.lock();
    if let Some(oldact) = oldact.check_non_null() {
        oldact.write_vm(actions[signo].clone().into())?;
    }
    if let Some(act) = act.check_non_null() {
        let act = act.read_vm()?.into();
        debug!("sys_rt_sigaction <= signo: {signo:?}, act: {act:?}");
        actions[signo] = act;
    }
    Ok(0)
}

/// Returns the set of pending signals.
///
/// See <https://man7.org/linux/man-pages/man2/rt_sigpending.2.html>.
pub fn sys_rt_sigpending(set: UserPtr<k_sigset>, sigsetsize: usize) -> KResult<isize> {
    check_sigset_size(sigsetsize)?;
    set.write_vm(kthread::current_thread().signal.pending().into())?;
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
        kthread::current_thread().pid(),
    )))
}

/// Sends a signal to a process or process group.
///
/// See <https://man7.org/linux/man-pages/man2/kill.2.html>.
pub fn sys_kill(pid: i32, signo: u32) -> KResult<isize> {
    debug!("sys_kill: pid = {pid}, signo = {signo}");
    let sig = make_siginfo(signo, SI_USER as _)?;

    match pid {
        1.. => send_signal_to_process(pid as _, sig)?,
        0 => {
            let pgid = kthread::current_thread()
                .process_state()
                .proc
                .group()
                .pgid();
            send_signal_to_process_group(pgid, sig)?;
        }
        -1 => {
            let current_pid = kthread::current_thread().pid();
            if let Some(sig) = sig {
                for proc_state in processes() {
                    if proc_state.proc.is_init() || proc_state.proc.pid() == current_pid {
                        continue;
                    }
                    let _ = send_signal_to_process(proc_state.proc.pid(), Some(sig.clone()));
                }
            }
        }
        ..-1 => send_signal_to_process_group((-pid) as Pid, sig)?,
    }
    Ok(0)
}

/// Sends a signal to a specific thread.
///
/// See <https://man7.org/linux/man-pages/man2/tkill.2.html>.
pub fn sys_tkill(tid: Pid, signo: u32) -> KResult<isize> {
    let sig = make_siginfo(signo, SI_TKILL)?;
    send_signal_to_thread(None, tid, sig)?;
    Ok(0)
}

/// Sends a signal to a thread within a specific thread group.
///
/// See <https://man7.org/linux/man-pages/man2/tgkill.2.html>.
pub fn sys_tgkill(tgid: Pid, tid: Pid, signo: u32) -> KResult<isize> {
    let sig = make_siginfo(signo, SI_TKILL)?;
    send_signal_to_thread(Some(tgid), tid, sig)?;
    Ok(0)
}

/// Builds a queued signal payload for `rt_sigqueueinfo`-style syscalls.
pub fn make_queue_signal_info(
    tgid: Pid,
    signo: u32,
    sig: UserConstPtr<k_siginfo>,
) -> KResult<Option<SignalInfo>> {
    if signo == 0 {
        return Ok(None);
    }

    let signo = parse_signo(signo)?;
    let mut sig: SignalInfo = sig.read_vm()?.into();
    sig.set_signo(signo);
    if kthread::current_thread().pid() != tgid && (sig.code() >= 0 || sig.code() == SI_TKILL) {
        return Err(KError::OperationNotPermitted);
    }
    Ok(Some(sig))
}

/// Queues a real-time signal with additional information to a process.
pub fn sys_rt_sigqueueinfo(
    tgid: Pid,
    signo: u32,
    sig: UserConstPtr<k_siginfo>,
    sigsetsize: usize,
) -> KResult<isize> {
    check_sigset_size(sigsetsize)?;

    let sig = make_queue_signal_info(tgid, signo, sig)?;
    send_signal_to_process(tgid, sig)?;
    Ok(0)
}

/// Queues a real-time signal with additional information to a specific thread.
pub fn sys_rt_tgsigqueueinfo(
    tgid: Pid,
    tid: Pid,
    signo: u32,
    sig: UserConstPtr<k_siginfo>,
    sigsetsize: usize,
) -> KResult<isize> {
    check_sigset_size(sigsetsize)?;

    let sig = make_queue_signal_info(tgid, signo, sig)?;
    send_signal_to_thread(Some(tgid), tid, sig)?;
    Ok(0)
}

/// Returns from a signal handler and restores context.
///
/// See <https://man7.org/linux/man-pages/man2/sigreturn.2.html>.
pub fn sys_rt_sigreturn(uctx: &mut UserContext) -> KResult<isize> {
    block_next_signal();
    kthread::current_thread().signal.restore(uctx);
    Ok(uctx.retval() as isize)
}

/// Waits for a signal from a specified set with an optional timeout.
///
/// See <https://man7.org/linux/man-pages/man2/rt_sigtimedwait.2.html>.
pub fn sys_rt_sigtimedwait(
    uctx: &mut UserContext,
    set: UserConstPtr<k_sigset>,
    info: UserPtr<k_siginfo>,
    timeout: UserConstPtr<timespec>,
    sigsetsize: usize,
) -> KResult<isize> {
    check_sigset_size(sigsetsize)?;

    let set: SignalSet = set.read_vm()?.into();
    let timeout = if let Some(ts) = timeout.check_non_null() {
        let ts = ts.read_vm()?;
        Some(ts.try_into_time_value()?)
    } else {
        None
    };

    debug!("sys_rt_sigtimedwait => set = {set:?}, timeout = {timeout:?}");

    let current = current();
    let current_thread = kthread::current_thread();
    let signal = &current_thread.signal;

    let old_blocked = signal.blocked();
    signal.set_blocked(old_blocked & !set);

    uctx.set_retval(-LinuxError::EINTR.into_raw() as usize);
    let wait_signal = poll_fn(|cx| {
        if let Some(sig) = signal.dequeue_signal(&set) {
            signal.set_blocked(old_blocked);
            Poll::Ready(Some(sig))
        } else if check_signals(&current_thread, uctx, Some(old_blocked)) {
            Poll::Ready(None)
        } else {
            let _ = current.poll_interrupt(cx);
            Poll::Pending
        }
    });

    let Ok(sig) = block_on(future::timeout(timeout, wait_signal)) else {
        signal.set_blocked(old_blocked);
        return Err(KError::WouldBlock);
    };
    let Some(sig) = sig else {
        return Ok(0);
    };
    let signo = sig.signo();

    if let Some(info) = info.check_non_null() {
        info.write_vm(sig.into())?;
    }

    Ok(signo as _)
}

/// Replaces the signal mask and suspends execution until a signal is delivered.
///
/// See <https://man7.org/linux/man-pages/man2/sigsuspend.2.html>.
pub fn sys_rt_sigsuspend(
    uctx: &mut UserContext,
    set: UserConstPtr<k_sigset>,
    sigsetsize: usize,
) -> KResult<isize> {
    check_sigset_size(sigsetsize)?;

    let current = current();
    let current_thread = kthread::current_thread();

    let set: SignalSet = set.read_vm()?.into();
    let old_blocked = current_thread.signal.set_blocked(set);

    uctx.set_retval(-LinuxError::EINTR.into_raw() as usize);
    block_on(poll_fn(|cx| {
        if check_signals(&current_thread, uctx, Some(old_blocked)) {
            return Poll::Ready(());
        }
        let _ = current.poll_interrupt(cx);
        Poll::Pending
    }));

    Err(KError::Interrupted)
}

/// Sets or retrieves the alternate signal stack.
///
/// See <https://man7.org/linux/man-pages/man2/sigaltstack.2.html>.
pub fn sys_sigaltstack(
    ss: UserConstPtr<k_sigaltstack>,
    old_ss: UserPtr<k_sigaltstack>,
) -> KResult<isize> {
    let signal = &kthread::current_thread().signal;

    if let Some(old_ss) = old_ss.check_non_null() {
        old_ss.write_vm(signal.stack().into())?;
    }

    if let Some(ss) = ss.check_non_null() {
        let ss: SignalStack = ss.read_vm()?.into();
        if ss.size <= MINSIGSTKSZ as usize {
            return Err(KError::NoMemory);
        }
        signal.set_stack(ss);
    }
    Ok(0)
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Generic signal-management syscalls.

use core::{future::poll_fn, task::Poll};

use kerrno::{KError, KResult, LinuxError};
use kfd_objects::signalfd::{Signalfd, SignalfdFlags};
use khal::uspace::UserContext;
use kprocess::Pid;
use ksignal::{SignalInfo, SignalSet, SignalStack, Signo};
use linux_raw_sys::general::{
    MINSIGSTKSZ, O_NONBLOCK, O_RDWR, SI_TKILL, SI_USER, SIG_BLOCK, SIG_SETMASK, SIG_UNBLOCK,
    SS_DISABLE, SS_ONSTACK, timespec,
};
use posix_process::check_signals;
use posix_types::{
    TimeValueLike, UserConstPtr, UserPtr, k_sigaction, k_sigaltstack, k_siginfo, k_sigset,
};

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
    posix_types::check_sigset_size(sigsetsize)?;

    let current_thread = kprocess::current_user_thread();
    let signal = current_thread.signal_manager();
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
    posix_types::check_sigset_size(sigsetsize)?;

    let signo = parse_signo(signo)?;
    if matches!(signo, Signo::SIGKILL | Signo::SIGSTOP) {
        return Err(KError::InvalidInput);
    }

    let current_thread = kprocess::current_user_thread();
    let new_action = act
        .check_non_null()
        .map(UserConstPtr::read_vm)
        .transpose()?
        .map(Into::into);
    debug!("sys_rt_sigaction <= signo: {signo:?}, act: {new_action:?}");
    let old_action = current_thread
        .process()
        .signal_manager()?
        .replace_signal_action(signo, new_action);

    if let Some(oldact) = oldact.check_non_null() {
        oldact.write_vm(old_action.into())?;
    }
    Ok(0)
}

/// Returns the set of pending signals.
///
/// See <https://man7.org/linux/man-pages/man2/rt_sigpending.2.html>.
pub fn sys_rt_sigpending(set: UserPtr<k_sigset>, sigsetsize: usize) -> KResult<isize> {
    posix_types::check_sigset_size(sigsetsize)?;
    set.write_vm(
        kprocess::current_user_thread()
            .signal_manager()
            .pending()
            .into(),
    )?;
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
        kprocess::current_user_thread().pid(),
    )))
}

/// Sends a signal to a process or process group.
///
/// See <https://man7.org/linux/man-pages/man2/kill.2.html>.
pub fn sys_kill(pid: i32, signo: u32) -> KResult<isize> {
    debug!("sys_kill: pid = {pid}, signo = {signo}");
    let sig = make_siginfo(signo, SI_USER as _)?;

    match pid {
        1.. => kprocess::process_signals::send_to_process(pid as _, sig)?,
        0 => {
            let pgid = kprocess::current_user_thread().process().group().pgid();
            kprocess::process_signals::send_to_process_group(pgid, sig)?;
        }
        -1 => {
            let current_pid = kprocess::current_user_thread().pid();
            let mut first_error = None;
            let targets = kprocess::process_signals::broadcast_process_targets(current_pid);

            for proc in &targets {
                if let Err(err) =
                    kprocess::process_signals::send_to_process(proc.pid(), sig.clone())
                {
                    debug!("sys_kill(-1) failed for pid {}: {err:?}", proc.pid());
                    if err != KError::OperationNotPermitted && first_error.is_none() {
                        first_error = Some(err);
                    }
                }
            }

            if targets.is_empty() {
                return Err(KError::NoSuchProcess);
            }
            if let Some(err) = first_error {
                return Err(err);
            }
        }
        ..-1 => {
            let pgid = pid.checked_neg().ok_or(KError::InvalidInput)? as Pid;
            kprocess::process_signals::send_to_process_group(pgid, sig)?;
        }
    }
    Ok(0)
}

/// Sends a signal to a specific thread.
///
/// See <https://man7.org/linux/man-pages/man2/tkill.2.html>.
pub fn sys_tkill(tid: Pid, signo: u32) -> KResult<isize> {
    let sig = make_siginfo(signo, SI_TKILL)?;
    kprocess::process_signals::send_to_thread(None, tid, sig)?;
    Ok(0)
}

/// Sends a signal to a thread within a specific thread group.
///
/// See <https://man7.org/linux/man-pages/man2/tgkill.2.html>.
pub fn sys_tgkill(tgid: Pid, tid: Pid, signo: u32) -> KResult<isize> {
    let sig = make_siginfo(signo, SI_TKILL)?;
    kprocess::process_signals::send_to_thread(Some(tgid), tid, sig)?;
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
    if kprocess::current_user_thread().pid() != tgid && (sig.code() >= 0 || sig.code() == SI_TKILL)
    {
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
    posix_types::check_sigset_size(sigsetsize)?;

    let sig = make_queue_signal_info(tgid, signo, sig)?;
    kprocess::process_signals::send_to_process(tgid, sig)?;
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
    posix_types::check_sigset_size(sigsetsize)?;

    let sig = make_queue_signal_info(tgid, signo, sig)?;
    kprocess::process_signals::send_to_thread(Some(tgid), tid, sig)?;
    Ok(0)
}

/// Returns from a signal handler and restores context.
///
/// See <https://man7.org/linux/man-pages/man2/sigreturn.2.html>.
pub fn sys_rt_sigreturn(uctx: &mut UserContext) -> KResult<isize> {
    kprocess::current_user_thread()
        .signal_manager()
        .restore(uctx);
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
    posix_types::check_sigset_size(sigsetsize)?;

    let set: SignalSet = set.read_vm()?.into();
    let timeout = if let Some(ts) = timeout.check_non_null() {
        let ts = ts.read_vm()?;
        Some(ts.try_into_time_value()?)
    } else {
        None
    };

    debug!("sys_rt_sigtimedwait => set = {set:?}, timeout = {timeout:?}");

    let current = ktask::current();
    let current_thread = kprocess::current_user_thread();
    let signal = current_thread.signal_manager();

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

    let Ok(sig) = ktask::future::block_on(ktask::future::timeout(timeout, wait_signal)) else {
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
    _uctx: &mut UserContext,
    set: UserConstPtr<k_sigset>,
    sigsetsize: usize,
) -> KResult<isize> {
    posix_types::check_sigset_size(sigsetsize)?;

    let current = ktask::current();
    let current_thread = kprocess::current_user_thread();

    let set: SignalSet = set.read_vm()?.into();
    let signal = current_thread.signal_manager();
    let old_blocked = signal.set_blocked(set);
    signal.set_saved_sigmask(old_blocked);

    ktask::future::block_on(poll_fn(|cx| {
        if !(signal.pending() & !signal.blocked()).is_empty() {
            return Poll::Ready(());
        }
        let _ = current.poll_interrupt(cx);
        Poll::Pending
    }));

    Err(KError::from(LinuxError::ERESTARTNOHAND))
}

/// Sets or retrieves the alternate signal stack.
///
/// See <https://man7.org/linux/man-pages/man2/sigaltstack.2.html>.
pub fn sys_sigaltstack(
    uctx: &UserContext,
    ss: UserConstPtr<k_sigaltstack>,
    old_ss: UserPtr<k_sigaltstack>,
) -> KResult<isize> {
    let current_thread = kprocess::current_user_thread();
    let signal = current_thread.signal_manager();
    let user_sp = uctx.sp();

    if let Some(old_ss) = old_ss.check_non_null() {
        old_ss.write_vm(signal.stack_for_sp(user_sp).into())?;
    }

    if let Some(ss) = ss.check_non_null() {
        if signal.is_on_signal_stack(user_sp) {
            return Err(KError::OperationNotPermitted);
        }

        let mut ss: SignalStack = ss.read_vm()?.into();
        match ss.flags {
            SS_DISABLE => {
                ss = SignalStack {
                    sp: 0,
                    flags: SS_DISABLE,
                    size: 0,
                };
            }
            0 | SS_ONSTACK => {}
            _ => return Err(KError::InvalidInput),
        }

        if ss.disabled() {
            ss = SignalStack {
                sp: 0,
                flags: SS_DISABLE,
                size: 0,
            };
        } else if ss.size < MINSIGSTKSZ as usize {
            return Err(KError::NoMemory);
        }

        signal.set_stack(ss);
    }
    Ok(0)
}

/// Creates or updates a `signalfd` file descriptor.
pub fn sys_signalfd4(
    fd: i32,
    mask: UserConstPtr<k_sigset>,
    sigsetsize: usize,
    flags: u32,
) -> KResult<isize> {
    posix_types::check_sigset_size(sigsetsize)?;

    let flags = SignalfdFlags::from_bits(flags).ok_or(KError::InvalidInput)?;

    if fd != -1 && flags.contains(SignalfdFlags::CLOEXEC) {
        return Err(KError::InvalidInput);
    }

    let mut mask: SignalSet = mask.read_vm()?.into();
    // SIGKILL and SIGSTOP cannot be caught, so they are silently removed
    // from the signalfd mask — matching Linux do_signalfd4 behavior.
    mask.remove(Signo::SIGKILL);
    mask.remove(Signo::SIGSTOP);

    if fd != -1 {
        let file = kprocess::current_resources().get_file(fd)?;
        let signalfd = Signalfd::from_file(&file)?;
        signalfd.update_mask(mask);
        return Ok(fd as _);
    }

    let file = Signalfd::new_file(
        mask,
        O_RDWR | (flags.bits() & O_NONBLOCK),
        kprocess::current_cred(),
    )?;

    kprocess::current_resources()
        .add_file(file, flags.contains(SignalfdFlags::CLOEXEC))
        .map(|fd| fd as _)
}

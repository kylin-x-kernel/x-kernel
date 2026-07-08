// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use kerrno::{KError, KResult};
use ksignal::SignalInfo;
use ktask::TaskInner;

use crate::{AsThread, Pid, Process, Thread, Tid, lookup};

fn send_signal_thread_inner(task: &TaskInner, thread: &Thread, sig: SignalInfo) {
    let signo = sig.signo();
    let signal = thread.signal_manager();
    if signal.send_signal(sig) && !signal.process().signal_ignored(signo) {
        task.interrupt();
    }
}

/// Sends a signal to a thread.
pub fn send_signal_to_thread(tgid: Option<Pid>, tid: Tid, sig: Option<SignalInfo>) -> KResult<()> {
    let task = lookup::task(tid)?;
    let thread = task.try_as_thread().ok_or(KError::OperationNotPermitted)?;
    if tgid.is_some_and(|tgid| thread.pid() != tgid) {
        return Err(KError::NoSuchProcess);
    }

    if let Some(sig) = sig {
        debug!("Send signal {:?} to thread {}", sig.signo(), tid);
        send_signal_thread_inner(&task, thread, sig);
    }

    Ok(())
}

/// Sends a signal to a process.
pub fn send_signal_to_process(pid: Pid, sig: Option<SignalInfo>) -> KResult<()> {
    let proc = lookup::live_process(pid)?;
    send_signal_to_process_ref(&proc, sig)
}

/// Sends a signal to the referenced process.
pub fn send_signal_to_process_ref(proc: &Arc<Process>, sig: Option<SignalInfo>) -> KResult<()> {
    let signal_manager = proc.signal_manager()?;

    if let Some(sig) = sig {
        let signo = sig.signo();
        debug!("Send signal {signo:?} to process {}", proc.pid());
        if let Some(tid) = signal_manager.send_signal(sig)
            && let Ok(task) = lookup::task(tid)
            && !signal_manager.signal_ignored(signo)
        {
            task.interrupt();
        }
    }

    Ok(())
}

/// Interrupts the task that currently represents the selected thread, if any.
pub fn interrupt_thread_by_tid(tid: Tid) -> KResult<()> {
    lookup::task(tid)?.interrupt();
    Ok(())
}

/// Sends a signal to a process group.
pub fn send_signal_to_process_group(pgid: Pid, sig: Option<SignalInfo>) -> KResult<()> {
    let pg = lookup::process_group(pgid)?;

    if let Some(sig) = sig {
        info!("Send signal {:?} to process group {}", sig.signo(), pgid);
        for proc in pg.processes() {
            send_signal_to_process(proc.pid(), Some(sig.clone()))?;
        }
    }

    Ok(())
}

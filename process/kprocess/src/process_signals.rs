// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use kerrno::{KError, KResult};
use ksignal::SignalInfo;
use ktask::{TaskInner, current};

use crate::{AsThread, Pid, Process, Thread, Tid, lookup};

fn send_signal_thread_inner(task: &TaskInner, thread: &Thread, sig: SignalInfo) {
    let signo = sig.signo();
    let signal = thread.signal_manager();
    if signal.send_signal(sig) && !signal.process().signal_ignored(signo) {
        task.interrupt();
    }
}

struct CurrentSignalDispatchImpl;

#[crate_interface::impl_interface]
impl ksignal::CurrentSignalDispatch for CurrentSignalDispatchImpl {
    fn send_sig_current(signo: ksignal::Signo) -> KResult<()> {
        let task = current();
        let thread = task.try_as_thread().ok_or(KError::OperationNotPermitted)?;
        send_signal_thread_inner(&task, thread, SignalInfo::new_kernel(signo));
        Ok(())
    }
}

/// Sends a signal to a process identified by PID.
pub fn send_to_process(pid: Pid, sig: Option<SignalInfo>) -> KResult<()> {
    let proc = lookup::live_process(pid)?;
    send_to_process_ref(&proc, sig)
}

/// Sends a signal to a specific process object reference.
pub fn send_to_process_ref(proc: &Arc<Process>, sig: Option<SignalInfo>) -> KResult<()> {
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

/// Sends a signal to a process group.
pub fn send_to_process_group(pgid: Pid, sig: Option<SignalInfo>) -> KResult<()> {
    let group = lookup::process_group(pgid)?;

    if let Some(sig) = sig {
        info!("Send signal {:?} to process group {}", sig.signo(), pgid);
        for proc in group.processes() {
            send_to_process(proc.pid(), Some(sig.clone()))?;
        }
    }

    Ok(())
}

/// Sends a signal to a thread.
pub fn send_to_thread(tgid: Option<Pid>, tid: Tid, sig: Option<SignalInfo>) -> KResult<()> {
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

/// Interrupts the current task backing the target thread, if present.
pub fn interrupt_thread(tid: Tid) -> KResult<()> {
    lookup::task(tid)?.interrupt();
    Ok(())
}

/// Returns non-zombie processes that should receive a broadcast process-directed signal.
pub fn broadcast_process_targets(excluded_pid: Pid) -> alloc::vec::Vec<Arc<Process>> {
    lookup::live_processes()
        .into_iter()
        .filter(|proc| !proc.is_init() && proc.pid() != excluded_pid)
        .collect()
}

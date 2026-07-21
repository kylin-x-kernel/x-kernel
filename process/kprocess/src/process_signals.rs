// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use kerrno::{KError, KResult};
use ksignal::{
    SigchldChildExitSignalInfo, SignalInfo,
    api::{PreparedChildExitSignal as PreparedKsignalChildExitSignal, ProcessSignalManager},
};
use ktask::{TaskInner, current};

use crate::{AsThread, Pid, Process, Thread, Tid, lookup};

fn send_signal_thread_inner(task: &TaskInner, thread: &Thread, sig: SignalInfo) {
    let signal = thread.signal_manager();
    if signal.send_signal(sig) {
        task.interrupt();
    }
}

/// A prepared child-exit signal whose queueing and task interrupt are delayed.
///
/// Exit code uses this to decide and perform autoreap before exposing SIGCHLD
/// or waking a parent task that may be blocked in `wait*()`.
pub struct PreparedChildExitSignal {
    signal_manager: Arc<ProcessSignalManager>,
    prepared: PreparedKsignalChildExitSignal,
}

impl PreparedChildExitSignal {
    /// Returns whether the child should be automatically reaped.
    pub fn should_autoreap(&self) -> bool {
        self.prepared.should_autoreap()
    }

    /// Queues SIGCHLD after exit state and autoreap decisions are visible.
    pub fn commit(self) -> Option<Tid> {
        self.signal_manager.commit_child_exit_signal(self.prepared)
    }

    /// Commits the signal and interrupts the parent thread selected at commit time.
    pub fn commit_and_interrupt(self) {
        if let Some(tid) = self.commit()
            && let Ok(task) = lookup::task(tid)
        {
            task.interrupt();
        }
    }
}

#[kiface::provide]
impl ksignal::CurrentSignalDispatch {
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
        {
            task.interrupt();
        }
    }

    Ok(())
}

/// Prepares child-exit `SIGCHLD` notification without publishing it yet.
pub fn prepare_child_exit_to_process_ref(
    proc: &Arc<Process>,
    sig: SigchldChildExitSignalInfo,
) -> KResult<PreparedChildExitSignal> {
    let signal_manager = proc.signal_manager()?;
    let prepared = signal_manager.prepare_child_exit_signal(sig);
    Ok(PreparedChildExitSignal {
        signal_manager,
        prepared,
    })
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

/// Returns non-exited processes that should receive a broadcast process-directed signal.
pub fn broadcast_process_targets(excluded_pid: Pid) -> alloc::vec::Vec<Arc<Process>> {
    lookup::live_processes()
        .into_iter()
        .filter(|proc| !proc.is_init() && proc.pid() != excluded_pid)
        .collect()
}

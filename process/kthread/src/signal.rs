// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kerrno::{KError, KResult};
use kprocess::Pid;
use ksignal::SignalInfo;
use ktask::TaskInner;

use crate::{AsThread, Thread, get_process_group, get_process_state, get_task};

fn send_signal_thread_inner(task: &TaskInner, thread: &Thread, sig: SignalInfo) {
    let signo = sig.signo();
    if thread.signal.send_signal(sig) {
        // Don't interrupt the target task for signals whose default
        // disposition is "ignore" (most notably SIGCHLD).  Such signals
        // should still be queued so that an explicit sigaction handler
        // or signalfd can observe them, but they must not kick a task
        // out of a blocking syscall (e.g. waitpid).
        if !thread.signal.process().signal_ignored(signo) {
            task.interrupt();
        }
    }
}

/// Sends a signal to a thread.
pub fn send_signal_to_thread(tgid: Option<Pid>, tid: Pid, sig: Option<SignalInfo>) -> KResult<()> {
    let task = get_task(tid)?;
    let thread = task.try_as_thread().ok_or(KError::OperationNotPermitted)?;
    if tgid.is_some_and(|tgid| thread.proc_state.proc.pid() != tgid) {
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
    let proc_state = get_process_state(pid)?;

    if let Some(sig) = sig {
        let signo = sig.signo();
        debug!("Send signal {signo:?} to process {pid}");
        if let Some(tid) = proc_state.signal.send_signal(sig)
            && let Ok(task) = get_task(tid)
            && !proc_state.signal.signal_ignored(signo)
        {
            task.interrupt();
        }
    }

    Ok(())
}

/// Sends a signal to a process group.
pub fn send_signal_to_process_group(pgid: Pid, sig: Option<SignalInfo>) -> KResult<()> {
    let pg = get_process_group(pgid)?;

    if let Some(sig) = sig {
        info!("Send signal {:?} to process group {}", sig.signo(), pgid);
        for proc in pg.processes() {
            send_signal_to_process(proc.pid(), Some(sig.clone()))?;
        }
    }

    Ok(())
}

#[cfg(unittest)]
mod tests_signal {
    use kerrno::KError;
    use ksignal::SignalInfo;
    use unittest::def_test;

    use super::{send_signal_to_process, send_signal_to_process_group, send_signal_to_thread};
    use crate::{
        cleanup_task_tables, get_process_group, get_process_state, get_session, get_task,
        processes, tasks,
    };

    #[def_test]
    fn test_cleanup_task_tables_on_empty_tables() {
        cleanup_task_tables();
    }

    #[def_test]
    fn test_task_tables_empty_queries_return_empty_or_not_found() {
        cleanup_task_tables();

        let _ = tasks();
        let _ = processes();
        assert!(matches!(get_task(12345), Err(KError::NoSuchProcess)));
        assert!(matches!(
            get_process_state(12345),
            Err(KError::NoSuchProcess)
        ));
        assert!(matches!(
            get_process_group(12345),
            Err(KError::NoSuchProcess)
        ));
        assert!(matches!(get_session(12345), Err(KError::NoSuchProcess)));
    }

    #[def_test]
    fn test_send_signal_helpers_propagate_missing_target_errors() {
        cleanup_task_tables();

        assert!(matches!(
            send_signal_to_thread(
                None,
                22222,
                Some(SignalInfo::new_kernel(ksignal::Signo::SIGTERM))
            ),
            Err(KError::NoSuchProcess)
        ));
        assert!(matches!(
            send_signal_to_process(22222, Some(SignalInfo::new_kernel(ksignal::Signo::SIGTERM))),
            Err(KError::NoSuchProcess)
        ));
        assert!(matches!(
            send_signal_to_process_group(
                22222,
                Some(SignalInfo::new_kernel(ksignal::Signo::SIGTERM))
            ),
            Err(KError::NoSuchProcess)
        ));
    }
}

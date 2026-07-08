// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use ksignal::SignalInfo;

use crate::{Pid, Process, Tid, lookup, signal};

/// Sends a signal to a process identified by PID.
pub fn send_to_process(pid: Pid, sig: Option<SignalInfo>) -> kerrno::KResult<()> {
    signal::send_signal_to_process(pid, sig)
}

/// Sends a signal to a specific process object reference.
pub fn send_to_process_ref(proc: &Arc<Process>, sig: Option<SignalInfo>) -> kerrno::KResult<()> {
    signal::send_signal_to_process_ref(proc, sig)
}

/// Sends a signal to a process group.
pub fn send_to_process_group(pgid: Pid, sig: Option<SignalInfo>) -> kerrno::KResult<()> {
    signal::send_signal_to_process_group(pgid, sig)
}

/// Sends a signal to a thread.
pub fn send_to_thread(tgid: Option<Pid>, tid: Tid, sig: Option<SignalInfo>) -> kerrno::KResult<()> {
    signal::send_signal_to_thread(tgid, tid, sig)
}

/// Interrupts the current task backing the target thread, if present.
pub fn interrupt_thread(tid: Tid) -> kerrno::KResult<()> {
    signal::interrupt_thread_by_tid(tid)
}

/// Returns non-zombie processes that should receive a broadcast process-directed signal.
pub fn broadcast_process_targets(excluded_pid: Pid) -> alloc::vec::Vec<Arc<Process>> {
    lookup::live_processes()
        .into_iter()
        .filter(|proc| !proc.is_init() && proc.pid() != excluded_pid)
        .collect()
}

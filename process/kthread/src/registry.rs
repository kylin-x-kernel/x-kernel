// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};

use kerrno::{KError, KResult};
use kprocess::{Pid, ProcessGroup, Session};
use ksync::{RwLock, static_lock};
use ktask::{KtaskRef, WeakKtaskRef, current};
use weak_map::WeakMap;

use crate::{AsThread, ProcessState, current_process_state};

static_lock! {
    static TASK_TABLE: RwLock<WeakMap<Pid, WeakKtaskRef>> = RwLock::new(WeakMap::new());
}
static_lock! {
    static PROCESS_TABLE: RwLock<WeakMap<Pid, Weak<ProcessState>>> = RwLock::new(WeakMap::new());
}
static_lock! {
    static PROCESS_GROUP_TABLE: RwLock<WeakMap<Pid, Weak<ProcessGroup>>> = RwLock::new(WeakMap::new());
}
static_lock! {
    static SESSION_TABLE: RwLock<WeakMap<Pid, Weak<Session>>> = RwLock::new(WeakMap::new());
}

/// Cleans up expired entries in the task tables.
pub fn cleanup_task_tables() {
    TASK_TABLE.write().cleanup();
    PROCESS_TABLE.write().cleanup();
    PROCESS_GROUP_TABLE.write().cleanup();
    SESSION_TABLE.write().cleanup();
}

/// Adds the task, process, process group, and session to the corresponding tables.
pub fn add_task_to_table(task: &KtaskRef) {
    let tid = task.id().as_u64() as Pid;

    let mut task_table = TASK_TABLE.write();
    task_table.insert(tid, task);

    let proc_state = &task.as_thread().proc_state;
    let proc = &proc_state.proc;
    let pid = proc.pid();
    let mut proc_table = PROCESS_TABLE.write();
    if proc_table.contains_key(&pid) {
        return;
    }
    proc_table.insert(pid, proc_state);

    let pg = proc.group();
    let mut pg_table = PROCESS_GROUP_TABLE.write();
    if pg_table.contains_key(&pg.pgid()) {
        return;
    }
    pg_table.insert(pg.pgid(), &pg);

    let session = pg.session();
    let mut session_table = SESSION_TABLE.write();
    if session_table.contains_key(&session.sid()) {
        return;
    }
    session_table.insert(session.sid(), &session);
}

/// Lists all tasks.
pub fn tasks() -> Vec<KtaskRef> {
    TASK_TABLE.read().values().collect()
}

/// Finds the task with the given TID.
pub fn get_task(tid: Pid) -> KResult<KtaskRef> {
    if tid == 0 {
        return Ok(current().clone());
    }
    TASK_TABLE.read().get(&tid).ok_or(KError::NoSuchProcess)
}

/// Lists all processes.
pub fn processes() -> Vec<Arc<ProcessState>> {
    PROCESS_TABLE.read().values().collect()
}

/// Finds the process with the given PID.
pub fn get_process_state(pid: Pid) -> KResult<Arc<ProcessState>> {
    if pid == 0 {
        return Ok(current_process_state());
    }
    PROCESS_TABLE.read().get(&pid).ok_or(KError::NoSuchProcess)
}

/// Finds the process group with the given PGID.
pub fn get_process_group(pgid: Pid) -> KResult<Arc<ProcessGroup>> {
    PROCESS_GROUP_TABLE
        .read()
        .get(&pgid)
        .ok_or(KError::NoSuchProcess)
}

/// Finds the session with the given SID.
pub fn get_session(sid: Pid) -> KResult<Arc<Session>> {
    SESSION_TABLE.read().get(&sid).ok_or(KError::NoSuchProcess)
}

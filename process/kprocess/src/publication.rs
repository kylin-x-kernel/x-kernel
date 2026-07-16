// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{
    collections::BTreeMap,
    sync::{Arc, Weak},
    vec::Vec,
};

use kerrno::{KError, KResult};
use ksync::RwLock;
use ktask::{KtaskRef, TaskInner, WeakKtaskRef, activate_task, current, prepare_task};
use weak_map::WeakMap;

use crate::{AsThread, Pid, Process, ProcessGroup, Session, Tid, current_user_process};

/// A prepared user task that is not yet visible to process/task registry lookups.
pub(crate) struct PreparedUserTask {
    task: KtaskRef,
}

/// A published user task that is visible to lookups but not yet runnable.
pub struct PublishedUserTask {
    task: KtaskRef,
    rollback: Option<PublicationRollback>,
}

pub(crate) struct PublicationRollback {
    process: Arc<Process>,
    pid: Pid,
    tid: Tid,
    insert_process: bool,
}

struct PublicationTables {
    task_table: WeakMap<Tid, WeakKtaskRef>,
    process_table: BTreeMap<Pid, Arc<Process>>,
    process_group_table: WeakMap<Pid, Weak<ProcessGroup>>,
    session_table: WeakMap<Pid, Weak<Session>>,
}

/// Global publication owner for the process domain.
pub(crate) struct ProcessPublication {
    tables: RwLock<PublicationTables>,
}

static PROCESS_PUBLICATION: ProcessPublication = ProcessPublication {
    tables: RwLock::new(PublicationTables {
        task_table: WeakMap::new(),
        process_table: BTreeMap::new(),
        process_group_table: WeakMap::new(),
        session_table: WeakMap::new(),
    }),
};

/// Returns the global process-publication owner.
pub(crate) fn process_publication() -> &'static ProcessPublication {
    &PROCESS_PUBLICATION
}

/// Prepares a user task for staged publication.
///
/// `TaskInner::new_user` installs the user-thread extension before this
/// function receives the task. The returned task is not yet visible through
/// process registries and will not execute until it is activated.
pub(crate) fn prepare_user_task(task: TaskInner) -> PreparedUserTask {
    let task = prepare_task(task);
    assert!(
        task_identity_matches_thread(&task),
        "prepare_user_task requires task identity and thread identity to match"
    );
    PreparedUserTask { task }
}

pub(crate) fn task_identity_matches_thread(task: &KtaskRef) -> bool {
    let Some(thread) = task.try_as_thread() else {
        return false;
    };
    let Some(task_number) = task.task_number() else {
        return false;
    };
    Arc::ptr_eq(task_number, thread.task_number())
}

impl PreparedUserTask {
    /// Publishes the prepared user task to task/process registries.
    pub(crate) fn publish(self) -> PublishedUserTask {
        let rollback = process_publication().publish_task(&self.task);
        PublishedUserTask {
            task: self.task,
            rollback: Some(rollback),
        }
    }
}

impl PublishedUserTask {
    /// Returns the published task reference before it becomes runnable.
    pub fn task(&self) -> &KtaskRef {
        &self.task
    }

    /// Runs parent-side completion work, then activates the task on success.
    ///
    /// If `finalize` returns an error, publication is rolled back before the
    /// error is returned to the caller.
    pub fn commit(mut self, finalize: impl FnOnce(&KtaskRef) -> KResult<()>) -> KResult<KtaskRef> {
        if let Err(err) = finalize(&self.task) {
            self.rollback_publication();
            return Err(err);
        }
        Ok(self.activate())
    }

    /// Aborts publication and removes the task from process/task visibility.
    pub fn abort(mut self) {
        self.rollback_publication();
    }

    /// Activates the published task, making it runnable.
    pub fn activate(mut self) -> KtaskRef {
        self.rollback.take();
        activate_task(&self.task);
        self.task.clone()
    }

    fn rollback_publication(&mut self) {
        if let Some(rollback) = self.rollback.take() {
            process_publication().rollback_task_publication(rollback);
        }
    }
}

impl Drop for PublishedUserTask {
    fn drop(&mut self) {
        self.rollback_publication();
    }
}

impl ProcessPublication {
    /// Cleans up expired task/process-group/session entries.
    pub(crate) fn cleanup(&self) {
        let mut tables = self.tables.write();
        tables.task_table.cleanup();
        tables.process_group_table.cleanup();
        tables.session_table.cleanup();
    }

    fn refresh_process_identity_locked(
        tables: &mut PublicationTables,
        proc: &Arc<Process>,
        insert_process: bool,
    ) {
        let pid = proc.pid();
        if insert_process {
            tables
                .process_table
                .entry(pid)
                .or_insert_with(|| proc.clone());
        }

        let pg = proc.group();
        tables.process_group_table.insert(pg.pgid(), &pg);

        let session = pg.session();
        tables.session_table.insert(session.sid(), &session);
    }

    /// Publishes a process identity together with its current group and session.
    #[cfg(unittest)]
    pub(crate) fn publish_process_identity(&self, proc: &Arc<Process>) {
        let mut tables = self.tables.write();
        Self::refresh_process_identity_locked(&mut tables, proc, true);
    }

    /// Refreshes process-group/session visibility for an already published process.
    pub(crate) fn refresh_job_control_identity(&self, proc: &Arc<Process>) {
        let mut tables = self.tables.write();
        Self::refresh_process_identity_locked(&mut tables, proc, false);
    }

    /// Publishes a task and its related process/group/session identities.
    pub(crate) fn publish_task(&self, task: &KtaskRef) -> PublicationRollback {
        let thread = task.as_thread();
        let proc = thread.process();
        let pid = proc.pid();
        let tid = thread.tid();
        let mut tables = self.tables.write();
        let insert_process = !tables.process_table.contains_key(&pid);
        proc.add_thread_task(task);
        Self::refresh_process_identity_locked(&mut tables, proc, insert_process);
        tables.task_table.insert(tid, task);
        PublicationRollback {
            process: proc.clone(),
            pid,
            tid,
            insert_process,
        }
    }

    fn rollback_task_publication(&self, rollback: PublicationRollback) {
        let mut tables = self.tables.write();
        tables.task_table.remove(&rollback.tid);
        if rollback.insert_process && !rollback.process.is_zombie() {
            tables.process_table.remove(&rollback.pid);
        }
        drop(tables);
        rollback.process.remove_thread_task(rollback.tid);
        if rollback.insert_process && !rollback.process.is_zombie() {
            rollback.process.discard_unpublished();
        }
    }

    /// Removes a reaped process identity from the PID directory.
    pub(crate) fn unpublish_process(&self, pid: Pid) {
        self.tables.write().process_table.remove(&pid);
    }

    /// Lists all published tasks.
    pub(crate) fn tasks(&self) -> Vec<KtaskRef> {
        self.tables.read().task_table.values().collect()
    }

    /// Finds the task with the given TID.
    pub(crate) fn task(&self, tid: Tid) -> KResult<KtaskRef> {
        if tid == 0 {
            return Ok(current().clone());
        }
        self.tables
            .read()
            .task_table
            .get(&tid)
            .ok_or(KError::NoSuchProcess)
    }

    /// Lists all published process identities, including zombies that have not
    /// yet been reaped from the PID directory.
    pub(crate) fn published_processes(&self) -> Vec<Arc<Process>> {
        self.tables.read().process_table.values().cloned().collect()
    }

    /// Returns the number of published process identities currently visible in
    /// the PID directory.
    pub(crate) fn published_process_count(&self) -> usize {
        self.tables.read().process_table.len()
    }

    /// Finds the published process identity with the given PID.
    pub(crate) fn published_process(&self, pid: Pid) -> KResult<Arc<Process>> {
        if pid == 0 {
            return Ok(current_user_process());
        }
        self.tables
            .read()
            .process_table
            .get(&pid)
            .cloned()
            .ok_or(KError::NoSuchProcess)
    }

    /// Finds the published process identity with the given PID and ensures it
    /// still represents a non-zombie process.
    ///
    /// "Live" is an external observation contract, not a statement about
    /// whether an internal runtime attachment can still be upgraded.
    pub(crate) fn live_process(&self, pid: Pid) -> KResult<Arc<Process>> {
        let process = self.published_process(pid)?;
        if !process.is_zombie() {
            Ok(process)
        } else {
            Err(KError::NoSuchProcess)
        }
    }

    /// Lists all published processes that are still externally observable as live.
    pub(crate) fn live_processes(&self) -> Vec<Arc<Process>> {
        self.published_processes()
            .into_iter()
            .filter(|process| !process.is_zombie())
            .collect()
    }

    /// Finds the process group with the given PGID.
    pub(crate) fn process_group(&self, pgid: Pid) -> KResult<Arc<ProcessGroup>> {
        self.tables
            .read()
            .process_group_table
            .get(&pgid)
            .ok_or(KError::NoSuchProcess)
    }
}

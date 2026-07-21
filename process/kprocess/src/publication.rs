// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{
    collections::BTreeMap,
    sync::{Arc, Weak},
    vec::Vec,
};

use kerrno::{KError, KResult};
use kspin::SpinNoIrq;
use ksync::RwLock;
use ktask::{KtaskRef, TaskInner, WeakKtaskRef, activate_task, current, prepare_task};

use crate::{
    AsThread, Pid, Process, ProcessGroup, ProcessGroupMemberSlot, Session, Tid,
    current_user_process, process_domain,
};

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
    thread_binding: ThreadPublicationBinding,
    process_effect: ProcessIdentityEffect,
}

pub(crate) type PublishedProcessSlot = PublicationSlot<Arc<Process>>;

enum PublicationSlotInner<T> {
    Vacant,
    Reserved,
    Published(T),
    Retired,
}

pub(crate) struct PublicationSlot<T> {
    inner: SpinNoIrq<PublicationSlotInner<T>>,
}

impl<T> PublicationSlot<T> {
    const fn new() -> Self {
        Self {
            inner: SpinNoIrq::new(PublicationSlotInner::Vacant),
        }
    }

    fn reserve_vacant_task_slot(&self) {
        let mut inner = self.inner.lock();
        assert!(
            matches!(
                &*inner,
                PublicationSlotInner::Vacant | PublicationSlotInner::Retired
            ),
            "cannot reserve an active task-publication slot"
        );
        *inner = PublicationSlotInner::Reserved;
    }

    fn reserve_identity_if_unpublished(&self) -> PublicationReservation {
        let mut inner = self.inner.lock();
        if matches!(&*inner, PublicationSlotInner::Published(_)) {
            PublicationReservation::AlreadyPublished
        } else {
            *inner = PublicationSlotInner::Reserved;
            PublicationReservation::ReservedByTransaction
        }
    }

    pub(crate) fn publish(&self, value: T) {
        let mut inner = self.inner.lock();
        assert!(
            matches!(
                &*inner,
                PublicationSlotInner::Reserved | PublicationSlotInner::Published(_)
            ),
            "publishing an unreserved process-publication slot"
        );
        *inner = PublicationSlotInner::Published(value);
    }

    pub(crate) fn retire(&self) {
        *self.inner.lock() = PublicationSlotInner::Retired;
    }

    fn can_cleanup(&self) -> bool {
        matches!(
            &*self.inner.lock(),
            PublicationSlotInner::Vacant | PublicationSlotInner::Retired
        )
    }

    fn is_published(&self) -> bool {
        matches!(&*self.inner.lock(), PublicationSlotInner::Published(_))
    }
}

impl<T: Clone> PublicationSlot<T> {
    pub(crate) fn snapshot(&self) -> Option<T> {
        let inner = self.inner.lock();
        match &*inner {
            PublicationSlotInner::Published(value) => Some(value.clone()),
            PublicationSlotInner::Vacant
            | PublicationSlotInner::Reserved
            | PublicationSlotInner::Retired => None,
        }
    }
}

struct PublicationTables {
    task_table: BTreeMap<Tid, Arc<PublicationSlot<WeakKtaskRef>>>,
    process_table: BTreeMap<Pid, Arc<PublicationSlot<Arc<Process>>>>,
    process_group_table: BTreeMap<Pid, Arc<PublicationSlot<Weak<ProcessGroup>>>>,
    session_table: BTreeMap<Pid, Arc<PublicationSlot<Weak<Session>>>>,
}

struct ProcessIdentitySlots {
    process: Arc<PublishedProcessSlot>,
    group: Arc<PublicationSlot<Weak<ProcessGroup>>>,
    session: Arc<PublicationSlot<Weak<Session>>>,
}

struct JobControlIdentitySlots {
    member: Arc<ProcessGroupMemberSlot>,
    group: PublicationSlotEffect<Weak<ProcessGroup>>,
    session: PublicationSlotEffect<Weak<Session>>,
}

/// Binds the global TID slot and per-process thread member slot for one task.
struct ThreadPublicationBinding {
    task_slot: Arc<PublicationSlot<WeakKtaskRef>>,
    member_slot: Arc<crate::process::ThreadMemberSlot>,
}

impl ThreadPublicationBinding {
    fn reserve(
        task_slot: Arc<PublicationSlot<WeakKtaskRef>>,
        member_slot: Arc<crate::process::ThreadMemberSlot>,
    ) -> Self {
        task_slot.reserve_vacant_task_slot();
        Self {
            task_slot,
            member_slot,
        }
    }

    fn publish(
        &self,
        domain: &process_domain::ProcessDomainWriteGuard<'_>,
        proc: &Process,
        task: &KtaskRef,
    ) {
        proc.publish_thread_task_locked(domain, &self.member_slot, task);
        self.task_slot.publish(Arc::downgrade(task));
    }

    fn retire(&self) {
        self.task_slot.retire();
        self.member_slot.retire();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProcessIdentityEffect {
    Inserted,
    AlreadyPublished,
}

impl ProcessIdentityEffect {
    fn reserve_if_unpublished(slot: &PublishedProcessSlot) -> Self {
        match slot.reserve_identity_if_unpublished() {
            PublicationReservation::ReservedByTransaction => Self::Inserted,
            PublicationReservation::AlreadyPublished => Self::AlreadyPublished,
        }
    }

    fn inserted(self) -> bool {
        matches!(self, Self::Inserted)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PublicationReservation {
    ReservedByTransaction,
    AlreadyPublished,
}

impl PublicationReservation {
    fn should_retire_on_abort(self) -> bool {
        matches!(self, Self::ReservedByTransaction)
    }
}

struct PublicationSlotEffect<T> {
    slot: Arc<PublicationSlot<T>>,
    reservation: PublicationReservation,
}

impl<T> PublicationSlotEffect<T> {
    fn reserve_if_unpublished(slot: Arc<PublicationSlot<T>>) -> Self {
        let reservation = slot.reserve_identity_if_unpublished();
        Self { slot, reservation }
    }

    fn publish(&self, value: T) {
        self.slot.publish(value);
    }

    fn retire_reserved(&self) {
        if self.reservation.should_retire_on_abort() {
            self.slot.retire();
        }
    }
}

/// Global publication owner for the process domain.
pub(crate) struct ProcessPublication {
    tables: RwLock<PublicationTables>,
}

static PROCESS_PUBLICATION: ProcessPublication = ProcessPublication {
    tables: RwLock::new(PublicationTables {
        task_table: BTreeMap::new(),
        process_table: BTreeMap::new(),
        process_group_table: BTreeMap::new(),
        session_table: BTreeMap::new(),
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
        tables.task_table.retain(|_, slot| {
            !slot.can_cleanup()
                && slot
                    .snapshot()
                    .is_none_or(|weak_task| weak_task.upgrade().is_some())
        });
        tables.process_table.retain(|_, slot| !slot.can_cleanup());
        tables.process_group_table.retain(|_, slot| {
            !slot.can_cleanup()
                && slot
                    .snapshot()
                    .is_none_or(|weak_group| weak_group.upgrade().is_some())
        });
        tables.session_table.retain(|_, slot| {
            !slot.can_cleanup()
                && slot
                    .snapshot()
                    .is_none_or(|weak_session| weak_session.upgrade().is_some())
        });
    }

    fn reserve_process_identity_locked(
        tables: &mut PublicationTables,
        proc: &Arc<Process>,
    ) -> (ProcessIdentitySlots, ProcessIdentityEffect) {
        let pid = proc.pid();
        let process_slot = tables
            .process_table
            .entry(pid)
            .or_insert_with(|| Arc::new(PublicationSlot::new()))
            .clone();

        let pg = proc.group();
        let group_slot = tables
            .process_group_table
            .entry(pg.pgid())
            .or_insert_with(|| Arc::new(PublicationSlot::new()))
            .clone();

        let session = pg.session();
        let session_slot = tables
            .session_table
            .entry(session.sid())
            .or_insert_with(|| Arc::new(PublicationSlot::new()))
            .clone();

        let process_effect = ProcessIdentityEffect::reserve_if_unpublished(&process_slot);
        let _group_reservation = group_slot.reserve_identity_if_unpublished();
        let _session_reservation = session_slot.reserve_identity_if_unpublished();
        (
            ProcessIdentitySlots {
                process: process_slot,
                group: group_slot,
                session: session_slot,
            },
            process_effect,
        )
    }

    fn publish_process_identity_slots(
        domain: &process_domain::ProcessDomainWriteGuard<'_>,
        proc: &Arc<Process>,
        slots: &ProcessIdentitySlots,
        process_effect: ProcessIdentityEffect,
    ) {
        if process_effect.inserted() {
            slots.process.publish(proc.clone());
            proc.install_pid_publication_slot_locked(domain, &slots.process);
        }

        let pg = proc.group();
        proc.publish_group_membership_locked(domain);
        slots.group.publish(Arc::downgrade(&pg));

        let session = pg.session();
        slots.session.publish(Arc::downgrade(&session));
    }

    fn reserve_job_control_identity_locked(
        tables: &mut PublicationTables,
        proc: &Arc<Process>,
        group: &Arc<ProcessGroup>,
    ) -> JobControlIdentitySlots {
        let member = group.reserve_process_slot(proc.pid());
        let group_slot = tables
            .process_group_table
            .entry(group.pgid())
            .or_insert_with(|| Arc::new(PublicationSlot::new()))
            .clone();
        let session = group.session();
        let session_slot = tables
            .session_table
            .entry(session.sid())
            .or_insert_with(|| Arc::new(PublicationSlot::new()))
            .clone();

        JobControlIdentitySlots {
            member,
            group: PublicationSlotEffect::reserve_if_unpublished(group_slot),
            session: PublicationSlotEffect::reserve_if_unpublished(session_slot),
        }
    }

    /// Moves a process to `group` and publishes the corresponding group/session
    /// identities as one process-domain transaction.
    pub(crate) fn move_process_to_group(
        &self,
        proc: &Arc<Process>,
        group: &Arc<ProcessGroup>,
        allow_new_session: bool,
    ) -> bool {
        let mut tables = self.tables.write();
        let slots = Self::reserve_job_control_identity_locked(&mut tables, proc, group);
        let domain = process_domain::write_lock();
        let current_group = proc.group();
        if Arc::ptr_eq(&current_group, group) {
            slots.group.publish(Arc::downgrade(group));
            slots.session.publish(Arc::downgrade(&group.session()));
            return true;
        }
        if !allow_new_session && !Arc::ptr_eq(&current_group.session(), &group.session()) {
            slots.member.retire();
            slots.group.retire_reserved();
            slots.session.retire_reserved();
            return false;
        }

        proc.install_group_membership_locked(&domain, group, slots.member);
        slots.group.publish(Arc::downgrade(group));
        slots.session.publish(Arc::downgrade(&group.session()));
        true
    }

    /// Publishes a process identity together with its current group and session.
    #[cfg(unittest)]
    pub(crate) fn publish_process_identity(&self, proc: &Arc<Process>) {
        let mut tables = self.tables.write();
        let (slots, process_effect) = Self::reserve_process_identity_locked(&mut tables, proc);
        let domain = process_domain::write_lock();
        Self::publish_process_identity_slots(&domain, proc, &slots, process_effect);
    }

    /// Publishes a process after forcing cleanup between reserve and commit.
    #[cfg(unittest)]
    pub(crate) fn publish_process_identity_after_cleanup_for_test(&self, proc: &Arc<Process>) {
        let (slots, process_effect) = {
            let mut tables = self.tables.write();
            Self::reserve_process_identity_locked(&mut tables, proc)
        };
        self.cleanup();
        let domain = process_domain::write_lock();
        Self::publish_process_identity_slots(&domain, proc, &slots, process_effect);
    }

    /// Publishes a task and its related process/group/session identities.
    pub(crate) fn publish_task(&self, task: &KtaskRef) -> PublicationRollback {
        let thread = task.as_thread();
        let proc = thread.process();
        let mut tables = self.tables.write();
        let task_slot = tables
            .task_table
            .entry(thread.tid())
            .or_insert_with(|| Arc::new(PublicationSlot::new()))
            .clone();
        let member_slot = proc.reserve_thread_member_slot(thread.tid());
        let (slots, process_effect) = Self::reserve_process_identity_locked(&mut tables, proc);
        let thread_binding = ThreadPublicationBinding::reserve(task_slot, member_slot);

        let domain = process_domain::write_lock();
        thread_binding.publish(&domain, proc, task);
        Self::publish_process_identity_slots(&domain, proc, &slots, process_effect);
        PublicationRollback {
            process: proc.clone(),
            thread_binding,
            process_effect,
        }
    }

    fn rollback_task_publication(&self, rollback: PublicationRollback) {
        {
            let domain = process_domain::write_lock();
            if rollback.process_effect.inserted() && !rollback.process.is_exited_locked(&domain) {
                rollback.process.clear_published_identity_locked(&domain);
            }
            rollback.thread_binding.retire();
            if rollback.process_effect.inserted() && !rollback.process.is_exited_locked(&domain) {
                rollback.process.discard_unpublished_locked(&domain);
            }
        }
    }

    /// Removes a reaped process identity from the PID directory.
    pub(crate) fn unpublish_process(&self, pid: Pid) {
        let slot = self.tables.read().process_table.get(&pid).cloned();
        if let Some(slot) = slot {
            let domain = process_domain::write_lock();
            if let Some(process) = slot.snapshot() {
                process.clear_published_identity_locked(&domain);
            } else {
                slot.retire();
            }
        }
    }

    /// Removes a reaped process identity only when the PID still names `proc`.
    pub(crate) fn unpublish_process_if_matches(&self, proc: &Arc<Process>) -> bool {
        let pid = proc.pid();
        let slot = self.tables.read().process_table.get(&pid).cloned();
        let Some(slot) = slot else {
            return false;
        };
        let removed = {
            let domain = process_domain::write_lock();
            if slot
                .snapshot()
                .is_some_and(|published| Arc::ptr_eq(&published, proc))
            {
                proc.clear_published_identity_locked(&domain);
                true
            } else {
                false
            }
        };
        if !removed {
            return false;
        }

        true
    }

    /// Lists all published tasks.
    pub(crate) fn tasks(&self) -> Vec<KtaskRef> {
        let slots: Vec<_> = self.tables.read().task_table.values().cloned().collect();
        let _domain = process_domain::read_lock();
        slots
            .into_iter()
            .filter_map(|slot| slot.snapshot())
            .filter_map(|weak_task| weak_task.upgrade())
            .collect()
    }

    /// Finds the task with the given TID.
    pub(crate) fn task(&self, tid: Tid) -> KResult<KtaskRef> {
        if tid == 0 {
            return Ok(current().clone());
        }
        let slot = self
            .tables
            .read()
            .task_table
            .get(&tid)
            .cloned()
            .ok_or(KError::NoSuchProcess)?;
        let _domain = process_domain::read_lock();
        slot.snapshot()
            .and_then(|weak_task| weak_task.upgrade())
            .ok_or(KError::NoSuchProcess)
    }

    /// Lists all published process identities, including zombies that have not
    /// yet been reaped from the PID directory.
    pub(crate) fn published_processes(&self) -> Vec<Arc<Process>> {
        let slots: Vec<_> = self.tables.read().process_table.values().cloned().collect();
        let _domain = process_domain::read_lock();
        slots
            .into_iter()
            .filter_map(|slot| slot.snapshot())
            .collect()
    }

    /// Returns the number of published process identities currently visible in
    /// the PID directory.
    pub(crate) fn published_process_count(&self) -> usize {
        let slots: Vec<_> = self.tables.read().process_table.values().cloned().collect();
        let _domain = process_domain::read_lock();
        slots.into_iter().filter(|slot| slot.is_published()).count()
    }

    /// Finds the published process identity with the given PID.
    pub(crate) fn published_process(&self, pid: Pid) -> KResult<Arc<Process>> {
        if pid == 0 {
            return Ok(current_user_process());
        }
        let slot = self
            .tables
            .read()
            .process_table
            .get(&pid)
            .cloned()
            .ok_or(KError::NoSuchProcess)?;
        let _domain = process_domain::read_lock();
        slot.snapshot().ok_or(KError::NoSuchProcess)
    }

    /// Finds the published process identity with the given PID and ensures it
    /// still represents a non-exited process.
    ///
    /// "Live" is an external observation contract, not a statement about
    /// whether an internal runtime attachment can still be upgraded.
    pub(crate) fn live_process(&self, pid: Pid) -> KResult<Arc<Process>> {
        let process = self.published_process(pid)?;
        if !process.is_exited() {
            Ok(process)
        } else {
            Err(KError::NoSuchProcess)
        }
    }

    /// Lists all published processes that are still externally observable as live.
    pub(crate) fn live_processes(&self) -> Vec<Arc<Process>> {
        self.published_processes()
            .into_iter()
            .filter(|process| !process.is_exited())
            .collect()
    }

    /// Finds the process group with the given PGID.
    pub(crate) fn process_group(&self, pgid: Pid) -> KResult<Arc<ProcessGroup>> {
        let slot = self
            .tables
            .read()
            .process_group_table
            .get(&pgid)
            .cloned()
            .ok_or(KError::NoSuchProcess)?;
        let _domain = process_domain::read_lock();
        slot.snapshot()
            .and_then(|weak_group| weak_group.upgrade())
            .ok_or(KError::NoSuchProcess)
    }
}

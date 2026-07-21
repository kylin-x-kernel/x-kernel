// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::{Arc, Weak};

use kerrno::{KError, KResult};
use kidentity::PidHandle;
use ksignal::Signo;
use kspin::SpinNoIrq;
use linked_list_r4l::{GetLinks, Links};

use super::{INIT_PROC, Process, ProcessExitState};
use crate::{
    ForkParent, Pid, ProcessGroup, ProcessGroupMemberSlot, Session, process_domain,
    publication::process_publication,
};

pub(super) struct WaitParentContract {
    parent: Weak<Process>,
    pub(super) exit_signal: Option<Signo>,
}

impl WaitParentContract {
    pub(super) fn new(parent: Option<&Arc<Process>>, exit_signal: Option<Signo>) -> Self {
        Self {
            parent: parent.map(Arc::downgrade).unwrap_or_default(),
            exit_signal,
        }
    }

    pub(super) fn parent(&self) -> Option<Arc<Process>> {
        self.parent.upgrade()
    }
}

pub(super) struct ParentRelation {
    wait_contract: SpinNoIrq<WaitParentContract>,
    current_child_slot: SpinNoIrq<Option<Arc<ChildRelationSlot>>>,
    init_reparent_slot: Option<Arc<ChildRelationSlot>>,
}

impl ParentRelation {
    pub(super) fn prepare(
        pid: Pid,
        parent: Option<&Arc<Process>>,
        exit_signal: Option<Signo>,
    ) -> Self {
        let init_reparent_slot = parent.and_then(|_| {
            INIT_PROC
                .get()
                .map(|_| Arc::new(ChildRelationSlot::new(pid)))
        });
        let current_child_slot = parent.map(|parent| {
            if let (Some(init), Some(init_slot)) = (INIT_PROC.get(), init_reparent_slot.as_ref())
                && Arc::ptr_eq(parent, init)
            {
                init_slot.clone()
            } else {
                Arc::new(ChildRelationSlot::new(pid))
            }
        });

        Self {
            wait_contract: SpinNoIrq::new(WaitParentContract::new(parent, exit_signal)),
            current_child_slot: SpinNoIrq::new(current_child_slot),
            init_reparent_slot,
        }
    }

    pub(super) fn parent(&self) -> Option<Arc<Process>> {
        self.wait_contract.lock().parent()
    }

    pub(super) fn snapshot(&self) -> (Option<Arc<Process>>, Option<Signo>) {
        let wait_contract = self.wait_contract.lock();
        (wait_contract.parent(), wait_contract.exit_signal)
    }

    pub(super) fn exit_signal(&self) -> Option<Signo> {
        self.wait_contract.lock().exit_signal
    }

    pub(super) fn current_slot(&self) -> Option<Arc<ChildRelationSlot>> {
        self.current_child_slot.lock().clone()
    }

    pub(super) fn slot_for_parent(&self, parent: &Arc<Process>) -> Option<Arc<ChildRelationSlot>> {
        if let Some(init) = INIT_PROC.get()
            && Arc::ptr_eq(parent, init)
        {
            self.init_reparent_slot.clone()
        } else {
            self.current_slot()
        }
    }

    pub(super) fn set_current_parent(
        &self,
        parent: &Arc<Process>,
        exit_signal: Option<Signo>,
        slot: Arc<ChildRelationSlot>,
    ) {
        *self.wait_contract.lock() = WaitParentContract::new(Some(parent), exit_signal);
        *self.current_child_slot.lock() = Some(slot);
    }

    pub(super) fn clear_reserved_slots(&self) {
        if let Some(slot) = self.current_slot() {
            slot.clear();
        }
        if let Some(slot) = &self.init_reparent_slot {
            slot.clear();
        }
    }
}

pub(super) struct ChildRelationSlot {
    pub(super) pid: Pid,
    child: SpinNoIrq<Option<Arc<Process>>>,
    links: Links<Self>,
}

impl ChildRelationSlot {
    pub(super) fn new(pid: Pid) -> Self {
        Self {
            pid,
            child: SpinNoIrq::new(None),
            links: Links::new(),
        }
    }

    pub(super) fn publish(&self, child: &Arc<Process>) {
        *self.child.lock() = Some(child.clone());
    }

    pub(super) fn clear(&self) {
        *self.child.lock() = None;
    }

    pub(super) fn snapshot(&self) -> Option<Arc<Process>> {
        self.child.lock().clone()
    }
}

impl GetLinks for ChildRelationSlot {
    type EntryType = Self;

    fn get_links(data: &Self::EntryType) -> &Links<Self::EntryType> {
        &data.links
    }
}

pub(super) struct GroupMembership {
    group: SpinNoIrq<Arc<ProcessGroup>>,
    member_slot: SpinNoIrq<Option<Arc<ProcessGroupMemberSlot>>>,
}

impl GroupMembership {
    pub(super) fn prepare(group: Arc<ProcessGroup>, pid: Pid) -> Self {
        let member_slot = Some(group.reserve_process_slot(pid));
        Self {
            group: SpinNoIrq::new(group),
            member_slot: SpinNoIrq::new(member_slot),
        }
    }

    pub(super) fn group(&self) -> Arc<ProcessGroup> {
        self.group.lock().clone()
    }

    pub(super) fn publish(&self, process: &Arc<Process>) {
        if let Some(slot) = self.member_slot.lock().as_ref() {
            slot.publish(process);
        }
    }

    pub(super) fn retire(&self) {
        if let Some(slot) = self.member_slot.lock().as_ref() {
            slot.retire();
        }
    }

    pub(super) fn install(
        &self,
        process: &Arc<Process>,
        group: &Arc<ProcessGroup>,
        target_member_slot: Arc<ProcessGroupMemberSlot>,
    ) {
        let mut current_group = self.group.lock();
        if Arc::ptr_eq(&current_group, group) {
            return;
        }

        if let Some(slot) = self.member_slot.lock().replace(target_member_slot.clone()) {
            slot.retire();
        }
        target_member_slot.publish(process);

        *current_group = group.clone();
    }
}

impl Process {
    /// The parent [`Process`].
    pub fn parent(&self) -> Option<Arc<Process>> {
        let _domain = process_domain::read_lock();
        self.parent_relation.parent()
    }

    /// The child [`Process`]es.
    pub fn children(&self) -> alloc::vec::Vec<Arc<Process>> {
        let _domain = process_domain::read_lock();
        self.children
            .lock()
            .iter()
            .filter_map(|slot| slot.snapshot())
            .collect()
    }

    pub(crate) fn fork_with_tree_parent(
        self: &Arc<Process>,
        leader_task_number: Arc<PidHandle>,
        parent_selection: ForkParent,
        requested_exit_signal: Option<Signo>,
    ) -> KResult<Arc<Process>> {
        let (parent, exit_signal) = if parent_selection == ForkParent::CallerParent {
            let _domain = process_domain::read_lock();
            let (parent, exit_signal) = self.parent_relation.snapshot();
            (parent.ok_or(KError::InvalidInput)?, exit_signal)
        } else {
            (self.clone(), requested_exit_signal)
        };

        let process =
            Self::prepare_with_task_number(leader_task_number, Some(parent.clone()), exit_signal);
        let domain = process_domain::write_lock();
        if parent_selection == ForkParent::CallerParent {
            let (current_parent, current_exit_signal) = self.parent_relation.snapshot();
            let current_parent = current_parent.ok_or(KError::InvalidInput)?;
            if !Arc::ptr_eq(&current_parent, &parent) || current_exit_signal != exit_signal {
                process.discard_prepared_locked(&domain);
                return Err(KError::InvalidInput);
            }
        }
        if parent.exit_state_locked(&domain) != ProcessExitState::Running {
            process.discard_prepared_locked(&domain);
            return Err(KError::InvalidInput);
        }
        process.attach_prepared_locked(&domain);
        Ok(process)
    }

    /// The [`ProcessGroup`] that the [`Process`] belongs to.
    pub fn group(&self) -> Arc<ProcessGroup> {
        self.group_membership.group()
    }

    pub(crate) fn install_group_membership_locked(
        self: &Arc<Self>,
        _domain: &process_domain::ProcessDomainWriteGuard<'_>,
        group: &Arc<ProcessGroup>,
        target_member_slot: Arc<ProcessGroupMemberSlot>,
    ) {
        self.group_membership
            .install(self, group, target_member_slot);
    }

    /// Creates a new [`Session`] and new [`ProcessGroup`] and moves the
    /// [`Process`] to it.
    ///
    /// If the [`Process`] is already a session leader, this method does
    /// nothing and returns `None`.
    ///
    /// Otherwise, it returns the new [`Session`] and [`ProcessGroup`].
    ///
    /// The caller has to ensure that the new [`ProcessGroup`] does not conflict
    /// with any existing [`ProcessGroup`]. Thus, the [`Process`] must not
    /// be a [`ProcessGroup`] leader.
    ///
    /// Checking [`Session`] conflicts is unnecessary.
    pub fn create_session(self: &Arc<Self>) -> Option<(Arc<Session>, Arc<ProcessGroup>)> {
        let pid = self.pid();
        {
            let _domain = process_domain::read_lock();
            if self.group_membership.group().session.sid() == pid {
                return None;
            }
        }

        let new_session = Session::new(pid);
        let new_group = ProcessGroup::new(pid, &new_session);
        if !process_publication().move_process_to_group(self, &new_group, true) {
            return None;
        }

        Some((new_session, new_group))
    }

    /// Creates a new [`ProcessGroup`] and moves the [`Process`] to it.
    ///
    /// If the [`Process`] is already a group leader, this method does nothing
    /// and returns `None`.
    ///
    /// Otherwise, it returns the new [`ProcessGroup`].
    ///
    /// The caller has to ensure that the new [`ProcessGroup`] does not conflict
    /// with any existing [`ProcessGroup`].
    pub fn create_group(self: &Arc<Self>) -> Option<Arc<ProcessGroup>> {
        let pid = self.pid();
        let session = {
            let _domain = process_domain::read_lock();
            let group = self.group_membership.group();
            if group.pgid() == pid {
                return None;
            }
            group.session.clone()
        };

        let new_group = ProcessGroup::new(pid, &session);
        if !process_publication().move_process_to_group(self, &new_group, false) {
            return None;
        }

        Some(new_group)
    }

    /// Moves the [`Process`] to a specified [`ProcessGroup`].
    ///
    /// Returns `true` if the move succeeded. The move failed if the
    /// [`ProcessGroup`] is not in the same [`Session`] as the [`Process`].
    ///
    /// If the [`Process`] is already in the specified [`ProcessGroup`], this
    /// method does nothing and returns `true`.
    pub fn move_to_group(self: &Arc<Self>, group: &Arc<ProcessGroup>) -> bool {
        process_publication().move_process_to_group(self, group, false)
    }

    pub(super) fn remove_child_from_parent_locked(
        domain: &process_domain::ProcessDomainWriteGuard<'_>,
        parent: &Process,
        child: &Process,
    ) -> bool {
        let Some(slot) = child.parent_relation.current_slot() else {
            return false;
        };
        if slot.pid != child.pid() {
            return false;
        }
        if !slot
            .snapshot()
            .is_some_and(|linked_child| core::ptr::eq(linked_child.as_ref(), child))
        {
            return false;
        }
        if !Self::remove_child_slot_from_parent_locked(domain, parent, &slot) {
            return false;
        }
        slot.clear();
        true
    }

    fn remove_child_slot_from_parent_locked(
        _domain: &process_domain::ProcessDomainWriteGuard<'_>,
        parent: &Process,
        slot: &Arc<ChildRelationSlot>,
    ) -> bool {
        let mut children = parent.children.lock();
        if !children
            .iter()
            .any(|listed| core::ptr::eq(listed, slot.as_ref()))
        {
            return false;
        }
        // SAFETY: `process_domain` serializes all child-list mutations. The
        // membership check above found `slot` linked in this exact parent's list,
        // and no concurrent mutation can unlink it before this removal.
        unsafe { children.remove(slot) }.is_some()
    }

    pub(crate) fn discard_unpublished_locked(
        &self,
        domain: &process_domain::ProcessDomainWriteGuard<'_>,
    ) {
        if let Some(parent) = self.parent_relation.parent() {
            Self::remove_child_from_parent_locked(domain, &parent, self);
        }
        self.retire_group_membership_locked(domain);
    }

    /// Discards a process that never completed external publication.
    #[cfg(any(test, unittest))]
    pub(crate) fn discard_unpublished(&self) {
        let domain = process_domain::write_lock();
        self.discard_unpublished_locked(&domain);
    }

    pub(super) fn discard_prepared_locked(
        &self,
        _domain: &process_domain::ProcessDomainWriteGuard<'_>,
    ) {
        self.parent_relation.clear_reserved_slots();
        self.retire_group_membership_locked(_domain);
    }

    pub(super) fn attach_to_parent_slot_locked(
        self: &Arc<Self>,
        _domain: &process_domain::ProcessDomainWriteGuard<'_>,
        parent: &Arc<Process>,
        exit_signal: Option<Signo>,
    ) {
        let slot = self
            .parent_relation
            .slot_for_parent(parent)
            .expect("child must reserve a relation slot for its next wait parent");
        slot.publish(self);
        self.parent_relation
            .set_current_parent(parent, exit_signal, slot.clone());
        parent.children.lock().push_back(slot);
    }
}

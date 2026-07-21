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
use ktask::{KtaskRef, WeakKtaskRef};

use super::Process;
use crate::{Tid, process_domain};

/// Published thread membership tracked per process.
#[derive(Default)]
pub(super) struct ThreadMembership {
    members: BTreeMap<Tid, Arc<ThreadMemberSlot>>,
}

impl ThreadMembership {
    pub(super) fn reserve_slot(&mut self, tid: Tid) -> Arc<ThreadMemberSlot> {
        self.members
            .entry(tid)
            .or_insert_with(|| Arc::new(ThreadMemberSlot::new()))
            .clone()
    }

    fn retire(&mut self, tid: Tid) {
        if let Some(slot) = self.members.get(&tid) {
            slot.retire();
        }
        self.prune_stale();
    }

    fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    fn thread_ids(&self) -> Vec<Tid> {
        self.members
            .iter()
            .filter_map(|(tid, slot)| slot.snapshot().map(|_| *tid))
            .collect()
    }

    fn tasks(&self) -> Vec<KtaskRef> {
        self.members
            .values()
            .filter_map(|slot| slot.snapshot())
            .collect()
    }

    fn published_count(&self) -> usize {
        self.members
            .values()
            .filter(|slot| slot.snapshot().is_some())
            .count()
    }

    fn contains_published_tid(&self, tid: Tid) -> bool {
        self.members
            .get(&tid)
            .is_some_and(|slot| slot.snapshot().is_some())
    }

    fn representative_task(&self) -> KResult<KtaskRef> {
        self.members
            .values()
            .find_map(|slot| slot.snapshot())
            .ok_or(KError::NoSuchProcess)
    }

    fn prune_stale(&mut self) {
        self.members.retain(|_, slot| slot.snapshot().is_some());
    }
}

/// Thread-group exit state tracked per process.
#[derive(Default)]
pub(super) struct ThreadGroupExitState {
    pub(super) exit_code: i32,
    pub(super) group_exited: bool,
}

pub(crate) struct ThreadMemberSlot {
    task: SpinNoIrq<Option<WeakKtaskRef>>,
}

impl ThreadMemberSlot {
    fn new() -> Self {
        Self {
            task: SpinNoIrq::new(None),
        }
    }

    pub(super) fn publish(&self, task: &KtaskRef) {
        *self.task.lock() = Some(Arc::downgrade(task));
    }

    pub(crate) fn retire(&self) {
        *self.task.lock() = None;
    }

    pub(super) fn snapshot(&self) -> Option<KtaskRef> {
        self.task.lock().as_ref().and_then(Weak::upgrade)
    }
}

impl Process {
    pub(crate) fn reserve_thread_member_slot(&self, tid: Tid) -> Arc<ThreadMemberSlot> {
        self.thread_membership.lock().reserve_slot(tid)
    }

    /// Publishes a thread task into this [`Process`]'s membership table.
    pub(crate) fn publish_thread_task_locked(
        &self,
        _domain: &process_domain::ProcessDomainWriteGuard<'_>,
        slot: &Arc<ThreadMemberSlot>,
        task: &KtaskRef,
    ) {
        slot.publish(task);
    }

    /// Removes a thread from this [`Process`] and sets the exit code if the
    /// group has not exited.
    ///
    /// Returns `true` if this was the last thread in the process.
    pub(crate) fn exit_thread(self: &Arc<Self>, tid: Tid, exit_code: i32) -> bool {
        {
            let mut group_exit = self.group_exit.lock();
            if !group_exit.group_exited {
                group_exit.exit_code = exit_code;
            }
        }

        let mut thread_membership = self.thread_membership.lock();
        thread_membership.retire(tid);
        thread_membership.is_empty()
    }

    /// Returns a snapshot of published thread IDs in this [`Process`].
    pub fn threads(&self) -> Vec<Tid> {
        self.thread_membership.lock().thread_ids()
    }

    /// Returns a snapshot of published thread tasks in this [`Process`].
    pub(crate) fn thread_tasks(&self) -> Vec<KtaskRef> {
        self.thread_membership.lock().tasks()
    }

    /// Returns the number of published threads currently attached to this [`Process`].
    pub(crate) fn thread_count(&self) -> usize {
        self.thread_membership.lock().published_count()
    }

    /// Returns whether `tid` still resolves to a published thread in this [`Process`].
    pub(crate) fn contains_published_tid(&self, tid: Tid) -> bool {
        self.thread_membership.lock().contains_published_tid(tid)
    }

    /// Returns the lowest-TID published task that currently represents this [`Process`].
    pub(crate) fn representative_task(&self) -> KResult<KtaskRef> {
        self.thread_membership.lock().representative_task()
    }

    /// Returns `true` if the [`Process`] is group exited.
    pub fn is_group_exited(&self) -> bool {
        self.group_exit.lock().group_exited
    }

    /// Marks the [`Process`] as group exited.
    pub(crate) fn group_exit(&self) {
        self.group_exit.lock().group_exited = true;
    }

    /// The exit code of the [`Process`].
    pub fn exit_code(&self) -> i32 {
        self.group_exit.lock().exit_code
    }
}

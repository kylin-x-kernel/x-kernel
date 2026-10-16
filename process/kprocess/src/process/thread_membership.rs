// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{
    collections::BTreeMap,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::Ordering;

use kerrno::{KError, KResult};
use kidentity::TaskIdentityToken;
use kspin::SpinNoIrq;
use ktask::{KtaskRef, WeakKtaskRef};

use super::Process;
use crate::{AsThread, Tid, process_domain, publication::process_publication};

/// Published tasks and unreleased group-leader membership tracked per process.
///
/// Members are keyed by the task's stable [`TaskIdentityToken`], not by its
/// current TID, so an `exec` leader takeover that moves a number does not have
/// to re-index or migrate membership.
#[derive(Default)]
pub(super) struct ThreadMembership {
    members: BTreeMap<TaskIdentityToken, Arc<ThreadMemberSlot>>,
}

impl ThreadMembership {
    pub(super) fn reserve_slot(&mut self, token: TaskIdentityToken) -> Arc<ThreadMemberSlot> {
        // Runtime kidentity allocation is monotonic and checked: numbers are
        // never recycled on exit/drop or wrapped at exhaustion. Fixed-number
        // replacement fixtures exercise stale identity guards, not allocation.
        self.members
            .entry(token)
            .or_insert_with(|| Arc::new(ThreadMemberSlot::new()))
            .clone()
    }

    fn exit_task(&mut self, task: &KtaskRef, is_leader: bool) {
        if let Some(slot) = self.members.get(&task.as_thread().identity_token()) {
            slot.exit_task(task, is_leader);
        }
        self.prune_stale();
    }

    fn has_no_published_tasks(&self) -> bool {
        self.members.values().all(|slot| slot.snapshot().is_none())
    }

    fn unreleased_count(&self) -> usize {
        self.members
            .values()
            .filter(|slot| slot.is_unreleased())
            .count()
    }

    fn release_exited_leader(&mut self) {
        self.members.retain(|_, slot| !slot.is_exited_leader());
    }

    fn thread_ids(&self) -> Vec<Tid> {
        self.members
            .values()
            .filter_map(|slot| slot.snapshot())
            .map(|task| task.as_thread().tid())
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
            .values()
            .filter_map(|slot| slot.snapshot())
            .any(|task| task.as_thread().tid() == tid)
    }

    fn representative_task(&self) -> KResult<KtaskRef> {
        self.members
            .values()
            .find_map(|slot| slot.snapshot())
            .ok_or(KError::NoSuchProcess)
    }

    fn prune_stale(&mut self) {
        self.members.retain(|_, slot| slot.is_unreleased());
    }
}

/// Thread-group exit state tracked per process.
#[derive(Default)]
pub(super) struct ThreadGroupExitState {
    pub(super) exit_code: i32,
    pub(super) group_exited: bool,
}

enum ThreadMemberState {
    Vacant,
    Published(WeakKtaskRef),
    // A leader exits before its parent consumes the process identity. Keep
    // membership without retaining a task/runtime or allowing TID lookup.
    ExitedLeader,
}

pub(crate) struct ThreadMemberSlot {
    state: SpinNoIrq<ThreadMemberState>,
}

impl ThreadMemberSlot {
    fn new() -> Self {
        Self {
            state: SpinNoIrq::new(ThreadMemberState::Vacant),
        }
    }

    pub(super) fn publish(&self, task: &KtaskRef) {
        *self.state.lock() = ThreadMemberState::Published(Arc::downgrade(task));
    }

    pub(crate) fn retire(&self) {
        *self.state.lock() = ThreadMemberState::Vacant;
    }

    fn exit_task(&self, task: &KtaskRef, is_leader: bool) {
        let mut state = self.state.lock();
        let published = match &*state {
            ThreadMemberState::Published(published) => published,
            ThreadMemberState::Vacant | ThreadMemberState::ExitedLeader => return,
        };
        // Do not retire a replacement task that reused this numeric TID.
        if !Weak::ptr_eq(published, &Arc::downgrade(task)) {
            return;
        }
        *state = if is_leader {
            ThreadMemberState::ExitedLeader
        } else {
            ThreadMemberState::Vacant
        };
    }

    fn is_unreleased(&self) -> bool {
        match &*self.state.lock() {
            ThreadMemberState::Vacant => false,
            ThreadMemberState::Published(task) => task.strong_count() != 0,
            ThreadMemberState::ExitedLeader => true,
        }
    }

    fn is_exited_leader(&self) -> bool {
        matches!(*self.state.lock(), ThreadMemberState::ExitedLeader)
    }

    pub(super) fn snapshot(&self) -> Option<KtaskRef> {
        match &*self.state.lock() {
            ThreadMemberState::Published(task) => task.upgrade(),
            ThreadMemberState::Vacant | ThreadMemberState::ExitedLeader => None,
        }
    }
}

impl Process {
    pub(crate) fn reserve_thread_member_slot(
        &self,
        token: TaskIdentityToken,
    ) -> Arc<ThreadMemberSlot> {
        self.thread_membership.lock().reserve_slot(token)
    }

    /// Moves the thread-group identity onto `task` after a non-leader `exec`.
    ///
    /// `task` must be the process's sole surviving published thread. The number
    /// directory entry moves once and the thread adopts the group number;
    /// membership and cgroup key on the stable identity and need no change.
    /// Signal routing still keys threads by number, so its registry entry is
    /// re-keyed here. Call in sleepable context, before the new image is
    /// installed; returns `false` when the precondition or directory move fails.
    pub fn assume_leader_identity(&self, task: &KtaskRef) -> bool {
        let thread = task.as_thread();
        let old_tid = thread.tid();
        let pid = self.pid();
        // Verify the sole-survivor invariant for the leader case too, so a
        // sibling published during the sweep is refused instead of being left
        // running against the old address space.
        if self.thread_count() != 1 || !self.contains_published_tid(old_tid) {
            return false;
        }
        if old_tid == pid {
            return true;
        }
        if !process_publication().move_task_number(task, old_tid, pid) {
            return false;
        }
        thread.adopt_task_number(self.leader_task_number.clone());
        thread.signal_manager().rebind_after_exec(pid);
        true
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

    /// Removes a task from the live view of this [`Process`], retires its global
    /// TID directory entry, and sets the exit code if the group has not exited.
    ///
    /// `task` is the exiting published task identity. Global TID directory
    /// removal is identity-checked against that object so a reused TID that
    /// already publishes a different live task is not retired by mistake.
    ///
    /// After this returns, `lookup::task(tid)` / `task_by_tid` /
    /// `send_to_thread` / `interrupt_thread` treat `tid` as gone even though the
    /// exiting task may still be running local teardown (fd/mm cleanup,
    /// `complete_process_exit`, `set_exit`). That early unpublish is intentional:
    /// it prevents fork-heavy workloads from retaining Published(dead Weak) TID
    /// slots, and matches the view that a thread which has left the process
    /// membership table is no longer a valid external TID target. Concurrent
    /// `tgkill`/timer delivery into that teardown window therefore observes
    /// `NoSuchProcess` and is dropped rather than waking a half-exited thread.
    ///
    /// The group leader's member slot remains `ExitedLeader` until wait/autoreap
    /// releases the process identity. Other members release at task exit. This
    /// lifetime view is separate from published tasks and never permits lookup
    /// or scheduling
    /// of the exited leader.
    ///
    /// Returns `true` if no published tasks remain in the process.
    /// Captures structural MM ownership before retirement; callers must finish
    /// user-memory cleanup first and perform no subsequent user access.
    pub(crate) fn exit_thread(self: &Arc<Self>, task: &KtaskRef, exit_code: i32) -> bool {
        task.as_thread().capture_exit_address_space();
        let tid = task.as_thread().tid();
        {
            let mut group_exit = self.group_exit.lock();
            // A `de_thread` sweep target dies of the execing thread's SIGKILL,
            // not of its own exit status, so it must not become the process exit
            // code.
            if !group_exit.group_exited && !task.as_thread().is_exec_sweep_target() {
                group_exit.exit_code = exit_code;
            }
        }

        let is_last = {
            let mut thread_membership = self.thread_membership.lock();
            // Check the real boundary, including direct kprocess test callers.
            #[cfg(unittest)]
            assert!(task.as_thread().has_exit_address_space());
            // Signal eligibility ends with live membership, not with the last
            // external reference to the exiting task or a later sibling exec.
            task.as_thread().signal_manager().exit();
            thread_membership.exit_task(task, tid == self.pid());
            thread_membership.has_no_published_tasks()
        };
        // Drop the global TID directory entry with the member-slot retire so
        // fork-heavy workloads do not retain Published(dead Weak) slots until a
        // later full-table cleanup sweep.
        process_publication().unpublish_task_if_matches(task);
        is_last
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

    /// Counts published members plus an exited leader not yet released by reap.
    pub(crate) fn unreleased_thread_count(&self) -> usize {
        self.thread_membership.lock().unreleased_count()
    }

    /// Releases retained leader membership at the process identity boundary.
    pub(crate) fn release_exited_thread_members_locked(
        &self,
        _domain: &process_domain::ProcessDomainWriteGuard<'_>,
    ) {
        self.thread_membership.lock().release_exited_leader();
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

    /// The parent-death signal recorded by `prctl(PR_SET_PDEATHSIG)` — the
    /// signal this process should receive when its parent dies first. Zero
    /// when unset. Storage only: the parent-exit delivery path is not wired
    /// up yet (see `core/ksyscall/docs/security.md`).
    pub fn pdeath_signal(&self) -> u32 {
        self.pdeath_signal.load(Ordering::Relaxed)
    }

    /// Records the parent-death signal for this process.
    pub fn set_pdeath_signal(&self, signo: u32) {
        self.pdeath_signal.store(signo, Ordering::Relaxed);
    }

    /// The exit code of the [`Process`].
    pub fn exit_code(&self) -> i32 {
        self.group_exit.lock().exit_code
    }
}

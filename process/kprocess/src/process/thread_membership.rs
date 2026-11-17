// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};
use core::{
    ops::Bound::{Excluded, Unbounded},
    sync::atomic::Ordering,
};

use kcred::Cred;
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
    is_last_thread_exit_reserved: bool,
    exited_sched_runtime_ns: u64,
    // Like Linux stats_lock, detects a live-to-exited handoff during a scan.
    // Wrapping is safe unless one scan spans 2^64 membership updates.
    sched_snapshot_seq: u64,
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

    fn exit_task(&mut self, task: &KtaskRef, leader_cred: Option<Arc<Cred>>) -> bool {
        let did_exit = self
            .members
            .get(&task.as_thread().identity_token())
            .is_some_and(|slot| slot.begin_exit(task, leader_cred));
        if did_exit {
            self.sched_snapshot_seq = self.sched_snapshot_seq.wrapping_add(1);
        }
        let is_last = did_exit && self.has_no_published_tasks();
        if did_exit && !is_last {
            self.finish_exit(task);
        }
        self.prune_stale();
        is_last
    }

    fn finish_exit(&mut self, task: &KtaskRef) {
        if self
            .members
            .get(&task.as_thread().identity_token())
            .is_some_and(|slot| slot.finish_exit(task))
        {
            self.sched_snapshot_seq = self.sched_snapshot_seq.wrapping_add(1);
            self.exited_sched_runtime_ns = self
                .exited_sched_runtime_ns
                .saturating_add(task.sum_exec_runtime_ns());
        }
    }

    fn finish_last_exit(&mut self) -> Option<KtaskRef> {
        if let Some(task) = self.members.values().find_map(|slot| slot.exiting_task()) {
            self.finish_exit(&task);
            self.prune_stale();
            Some(task)
        } else {
            None
        }
    }

    fn has_no_published_tasks(&self) -> bool {
        self.members.values().all(|slot| !slot.is_published())
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

    fn exited_leader_cred(&self) -> Option<Arc<Cred>> {
        self.members
            .values()
            .find_map(|slot| slot.exited_leader_cred())
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

    fn is_sole_published_tid(&self, tid: Tid) -> bool {
        let mut published = self.members.values().filter_map(|slot| slot.snapshot());
        matches!(
            (published.next(), published.next()),
            (Some(task), None) if task.as_thread().tid() == tid
        )
    }

    fn representative_task(&self) -> KResult<KtaskRef> {
        self.members
            .values()
            .find_map(|slot| slot.snapshot())
            .ok_or(KError::NoSuchProcess)
    }

    fn prune_stale(&mut self) {
        self.members.retain(|_, slot| slot.is_retained());
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
    // The last thread is no longer a lookup target but still runs process
    // cleanup; its scheduler runtime remains part of the group until publication.
    Exiting(WeakKtaskRef, Option<Arc<Cred>>),
    // A leader exits before its parent consumes the process identity. Keep
    // membership without retaining a task/runtime or allowing TID lookup, but
    // retain the leader's objective credentials: Linux keeps the zombie leader
    // and `capget()` on the thread-group ID observes those credentials even
    // after sibling threads have replaced it in every live-task lookup.
    ExitedLeader(Arc<Cred>),
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
        *self.state.lock() = ThreadMemberState::Published(KtaskRef::downgrade(task));
    }

    pub(crate) fn retire(&self) {
        *self.state.lock() = ThreadMemberState::Vacant;
    }

    fn begin_exit(&self, task: &KtaskRef, leader_cred: Option<Arc<Cred>>) -> bool {
        let mut state = self.state.lock();
        let published = match &*state {
            ThreadMemberState::Published(published) => published,
            ThreadMemberState::Vacant
            | ThreadMemberState::Exiting(..)
            | ThreadMemberState::ExitedLeader(_) => return false,
        };
        // Do not retire a replacement task that reused this numeric TID.
        if !WeakKtaskRef::ptr_eq(published, &KtaskRef::downgrade(task)) {
            return false;
        }
        *state = ThreadMemberState::Exiting(KtaskRef::downgrade(task), leader_cred);
        true
    }

    fn finish_exit(&self, task: &KtaskRef) -> bool {
        let mut state = self.state.lock();
        let ThreadMemberState::Exiting(exiting, _) = &*state else {
            return false;
        };
        if !WeakKtaskRef::ptr_eq(exiting, &KtaskRef::downgrade(task)) {
            return false;
        }
        let ThreadMemberState::Exiting(_, leader_cred) = &mut *state else {
            return false;
        };
        let leader_cred = leader_cred.take();
        *state = match leader_cred {
            Some(cred) => ThreadMemberState::ExitedLeader(cred),
            None => ThreadMemberState::Vacant,
        };
        true
    }

    fn is_unreleased(&self) -> bool {
        match &*self.state.lock() {
            ThreadMemberState::Vacant => false,
            ThreadMemberState::Published(task) => task.strong_count() != 0,
            ThreadMemberState::Exiting(_, leader_cred) => leader_cred.is_some(),
            ThreadMemberState::ExitedLeader(_) => true,
        }
    }

    fn is_retained(&self) -> bool {
        match &*self.state.lock() {
            ThreadMemberState::Vacant => false,
            ThreadMemberState::Published(task) => task.strong_count() != 0,
            ThreadMemberState::Exiting(..) | ThreadMemberState::ExitedLeader(_) => true,
        }
    }

    fn is_published(&self) -> bool {
        matches!(&*self.state.lock(), ThreadMemberState::Published(task) if task.strong_count() != 0)
    }

    fn is_exited_leader(&self) -> bool {
        matches!(*self.state.lock(), ThreadMemberState::ExitedLeader(_))
    }

    fn exited_leader_cred(&self) -> Option<Arc<Cred>> {
        match &*self.state.lock() {
            ThreadMemberState::ExitedLeader(cred) | ThreadMemberState::Exiting(_, Some(cred)) => {
                Some(cred.clone())
            }
            ThreadMemberState::Exiting(_, None) => None,
            ThreadMemberState::Vacant | ThreadMemberState::Published(_) => None,
        }
    }

    pub(super) fn snapshot(&self) -> Option<KtaskRef> {
        match &*self.state.lock() {
            ThreadMemberState::Published(task) => task.upgrade(),
            ThreadMemberState::Vacant
            | ThreadMemberState::Exiting(..)
            | ThreadMemberState::ExitedLeader(_) => None,
        }
    }

    fn accounted_task(&self) -> Option<KtaskRef> {
        match &*self.state.lock() {
            ThreadMemberState::Published(task) | ThreadMemberState::Exiting(task, _) => {
                task.upgrade()
            }
            ThreadMemberState::Vacant | ThreadMemberState::ExitedLeader(_) => None,
        }
    }

    fn exiting_task(&self) -> Option<KtaskRef> {
        match &*self.state.lock() {
            ThreadMemberState::Exiting(task, _) => task.upgrade(),
            _ => None,
        }
    }
}

impl Process {
    /// Moves the thread-group identity onto `task` after a non-leader `exec`.
    ///
    /// `task` must be the process's sole surviving published thread. The number
    /// directory entry moves once and the thread adopts the group number.
    /// Membership and cgroup key on the stable identity; only the old leader's
    /// retained `ExitedLeader` slot is dropped, because that slot describes a
    /// process identity the survivor now owns rather than an unreleased member.
    /// Signal routing still keys threads by number, so its registry entry is
    /// re-keyed here. Call in sleepable context, before the new image is
    /// installed; returns `false` when the precondition or directory move fails.
    pub fn assume_leader_identity(&self, task: &KtaskRef) -> bool {
        let thread = task.as_thread();
        let old_tid = thread.tid();
        let pid = self.pid();
        // Sample the count and the surviving number under one membership lock,
        // so a sibling published during the sweep is refused instead of being
        // left running against the old address space. This also checks the
        // leader case: the transition must not move an identity it already owns.
        if !self.thread_membership.lock().is_sole_published_tid(old_tid) {
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
        // The survivor now owns the group identity. If the old leader exited
        // before this takeover, its `ExitedLeader` slot was retained only for
        // `/proc` link and thread counts; keeping it would double-count the
        // single surviving member until process reap.
        self.thread_membership.lock().release_exited_leader();
        true
    }

    /// Publishes a thread task into this [`Process`]'s membership table.
    pub(crate) fn publish_thread_task_locked(
        &self,
        _domain: &process_domain::ProcessDomainWriteGuard<'_>,
        token: TaskIdentityToken,
        task: &KtaskRef,
    ) -> KResult<Arc<ThreadMemberSlot>> {
        let mut membership = self.thread_membership.lock();
        if membership.is_last_thread_exit_reserved {
            return Err(KError::NoSuchProcess);
        }
        let slot = membership.reserve_slot(token);
        slot.publish(task);
        membership.sched_snapshot_seq = membership.sched_snapshot_seq.wrapping_add(1);
        Ok(slot)
    }

    pub(crate) fn retire_thread_slot_locked(
        &self,
        _domain: &process_domain::ProcessDomainWriteGuard<'_>,
        slot: &ThreadMemberSlot,
    ) {
        let mut membership = self.thread_membership.lock();
        slot.retire();
        membership.sched_snapshot_seq = membership.sched_snapshot_seq.wrapping_add(1);
    }

    fn refresh_current_sched_runtime(&self) {
        if ktask::current_may_uninit().is_some_and(|current| {
            current
                .try_as_thread()
                .is_some_and(|thread| core::ptr::eq(thread.process().as_ref(), self))
        }) {
            ktask::refresh_current_execution_runtime_ns();
        }
    }

    /// Returns the scheduler runtime of all current and exited group members.
    ///
    /// The current task is refreshed locally; other running tasks contribute
    /// their most recently committed scheduler value. Membership changes cause
    /// a retry, preserving the live-to-exited handoff across bounded batches.
    /// Call in task context without a run-queue or membership lock.
    pub fn process_cpu_time(&self) -> ktime_types::TimeSpan {
        const BATCH_SIZE: usize = 4;
        self.refresh_current_sched_runtime();
        'snapshot: loop {
            let mut sequence = None;
            let mut cursor = None;
            let mut total = 0u64;
            loop {
                // Keep upgraded task owners until after the IRQ-safe lock is
                // released, including an owner that races the final release.
                let mut retained: [Option<KtaskRef>; BATCH_SIZE] = core::array::from_fn(|_| None);
                let membership = self.thread_membership.lock();
                match sequence {
                    Some(expected) if expected != membership.sched_snapshot_seq => {
                        drop(membership);
                        continue 'snapshot;
                    }
                    None => {
                        sequence = Some(membership.sched_snapshot_seq);
                        total = membership.exited_sched_runtime_ns;
                    }
                    Some(_) => {}
                }
                let start = cursor.map_or(Unbounded, Excluded);
                let mut members = membership.members.range((start, Unbounded));
                for entry in &mut retained {
                    let Some((&token, slot)) = members.next() else {
                        break;
                    };
                    cursor = Some(token);
                    if let Some(task) = slot.accounted_task() {
                        total = total.saturating_add(task.sum_exec_runtime_ns());
                        *entry = Some(task);
                    }
                }
                let has_more = members.next().is_some();
                drop(membership);
                drop(retained);
                if !has_more {
                    return ktime_types::TimeSpan::from_nanos(total);
                }
            }
        }
    }

    pub(super) fn finish_exiting_thread_runtime(&self) {
        self.refresh_current_sched_runtime();
        let exiting = self.thread_membership.lock().finish_last_exit();
        drop(exiting);
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
    /// Returns `true` only when this call removes the last published task and
    /// reserves process cleanup. The reservation rejects later task publication.
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

        // `real_cred()` takes a sleepable lock, so snapshot the leader credential
        // before entering the IRQ-safe membership table. The slot still decides
        // whether this task currently owns the leader identity before storing it.
        let leader_cred = (tid == self.pid()).then(|| task.as_thread().real_cred());
        if ktask::current_may_uninit().is_some_and(|current| current.ptr_eq(task)) {
            ktask::refresh_current_execution_runtime_ns();
        }

        let is_last = {
            let mut thread_membership = self.thread_membership.lock();
            // Check the real boundary, including direct kprocess test callers.
            #[cfg(unittest)]
            assert!(task.as_thread().has_exit_address_space());
            // Signal eligibility ends with live membership, not with the last
            // external reference to the exiting task or a later sibling exec.
            task.as_thread().signal_manager().exit();
            let is_last = thread_membership.exit_task(task, leader_cred);
            if is_last {
                thread_membership.is_last_thread_exit_reserved = true;
            }
            is_last
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

    /// Returns the exited group leader's objective credentials while its
    /// retained membership slot still names this process.
    ///
    /// Linux resolves `capget(2)` for the thread-group ID through the zombie
    /// leader, not through a surviving sibling. The slot disappears when the
    /// process is reaped or a non-leader `exec` takes over the group identity.
    pub(crate) fn exited_leader_cred(&self) -> Option<Arc<Cred>> {
        self.thread_membership.lock().exited_leader_cred()
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

    /// Returns whether `tid` is the only published thread in this [`Process`].
    ///
    /// Count and number are sampled under one membership lock. This is the
    /// shared precondition for replacing the group image and for an exec
    /// identity takeover; neither may proceed while a sibling is published.
    pub(crate) fn is_sole_published_tid(&self, tid: Tid) -> bool {
        self.thread_membership.lock().is_sole_published_tid(tid)
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

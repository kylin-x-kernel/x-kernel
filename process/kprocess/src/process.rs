// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process structure and lifecycle management.
mod exit;
mod runtime_access;
mod thread_membership;
mod tree;

use alloc::sync::{Arc, Weak};
use core::fmt;

use exit::{AtomicProcessExitState, ProcessExitState};
pub use exit::{
    ProcessExitPublication, WaitChildKind, WaitChildScan, WaitChildSelector, WaitReapMode,
    WaitedChild,
};
use kidentity::PidHandle;
use ksignal::Signo;
use kspin::SpinNoIrq;
use lazyinit::LazyInit;
use linked_list_r4l::List;
pub use runtime_access::{LiveAddressSpace, ProcessExecUpdate};
pub(crate) use thread_membership::ThreadMemberSlot;
use thread_membership::{ThreadGroupExitState, ThreadMembership};
use tree::{ChildRelationSlot, GroupMembership, ParentRelation};

use crate::{
    Pid, ProcessGroup, ProcessRuntime, Session, lifecycle::ProcessLifecycleState, process_domain,
    publication::PublishedProcessSlot,
};

/// A process.
pub struct Process {
    /// Stable process-leader PID handle. Future PID namespace work should keep
    /// namespace-specific numbers behind this identity object.
    leader_task_number: Arc<PidHandle>,
    /// Fast exited-state observation for pidfd/live lookup.
    ///
    /// Reads are advisory unless the caller also holds the process-domain
    /// transaction lock and is updating parent/children/publication state.
    exit_state: AtomicProcessExitState,
    thread_membership: SpinNoIrq<ThreadMembership>,
    group_exit: SpinNoIrq<ThreadGroupExitState>,
    lifecycle: ProcessLifecycleState,

    // TODO: child subreaper9
    /// Current children linked under this process. Mutate only under
    /// `process_domain` write lock.
    children: SpinNoIrq<List<Arc<ChildRelationSlot>>>,
    /// Wait parent contract and preallocated child-list relation slots. Mutate
    /// only under `process_domain` write lock.
    parent_relation: ParentRelation,

    /// Current process group and published membership slot. Group moves are
    /// committed through `ProcessPublication` so group membership and PGID/SID
    /// lookup publish together under `process_domain`.
    group_membership: GroupMembership,
    /// Weak runtime handle. A process identity can outlive its live runtime.
    runtime_ref: SpinNoIrq<Option<Weak<ProcessRuntime>>>,
    /// Published PID slot, retained so exit/reap can retire visibility without
    /// taking the publication table lock inside `process_domain`.
    pid_publication_slot: SpinNoIrq<Option<Weak<PublishedProcessSlot>>>,
}

impl Process {
    /// The [`Process`] ID.
    ///
    /// Returns the root/global PID number from the leader task-number handle.
    /// This is stable under the current global-PID semantics. Once
    /// `CLONE_NEWPID` is fully enabled across all syscall paths, callers that
    /// need a namespace-relative view should use `nr_in(ns)` on the underlying
    /// [`kidentity::PidHandle`] instead.
    pub fn pid(&self) -> Pid {
        self.leader_task_number.root_nr()
    }

    /// Returns the exit signal configured for this process.
    pub fn exit_signal(&self) -> Option<Signo> {
        let _domain = process_domain::read_lock();
        self.parent_relation.exit_signal()
    }

    /// Returns `true` if the [`Process`] is the init process.
    ///
    /// This is a convenience method for checking if the [`Process`]
    /// [`Arc::ptr_eq`]s with the init process, which is cheaper than
    /// calling [`init_proc`] or testing if [`Process::parent`] is `None`.
    pub fn is_init(self: &Arc<Self>) -> bool {
        INIT_PROC.get().is_some_and(|init| Arc::ptr_eq(self, init))
    }

    pub(crate) fn install_pid_publication_slot_locked(
        &self,
        _domain: &process_domain::ProcessDomainWriteGuard<'_>,
        slot: &Arc<PublishedProcessSlot>,
    ) {
        *self.pid_publication_slot.lock() = Some(Arc::downgrade(slot));
    }

    /// Returns the process-owned PID publication slot, if it is still allocated.
    ///
    /// Used by directory reap paths to delete a retired slot after wait/autoreap
    /// already cleared the published value, without racing a reused PID that
    /// installed a different slot.
    pub(crate) fn published_pid_slot(&self) -> Option<Arc<PublishedProcessSlot>> {
        self.pid_publication_slot
            .lock()
            .as_ref()
            .and_then(Weak::upgrade)
    }

    pub(crate) fn clear_published_identity_locked(
        &self,
        domain: &process_domain::ProcessDomainWriteGuard<'_>,
    ) {
        if let Some(slot) = self.published_pid_slot() {
            slot.retire();
        }
        self.retire_group_membership_locked(domain);
    }

    pub(crate) fn publish_group_membership_locked(
        self: &Arc<Self>,
        _domain: &process_domain::ProcessDomainWriteGuard<'_>,
    ) {
        self.group_membership.publish(self);
    }

    fn retire_group_membership_locked(
        &self,
        _domain: &process_domain::ProcessDomainWriteGuard<'_>,
    ) {
        self.group_membership.retire();
    }
}

impl fmt::Debug for Process {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let pid = self.pid();
        let is_exited = self.is_exited();
        let (group_exited, exit_code) = {
            let group_exit = self.group_exit.lock();
            (group_exit.group_exited, group_exit.exit_code)
        };
        let parent_pid = self.parent().map(|parent| parent.pid());
        let group = self.group();

        let mut builder = f.debug_struct("Process");
        builder.field("pid", &pid);

        if group_exited {
            builder.field("group_exited", &group_exited);
        }
        if is_exited {
            builder.field("exit_code", &exit_code);
        }

        if let Some(parent_pid) = parent_pid {
            builder.field("parent", &parent_pid);
        }
        builder.field("group", &group);
        builder.finish()
    }
}

/// Builder
impl Process {
    fn new_with_task_number(
        leader_task_number: Arc<PidHandle>,
        parent: Option<Arc<Process>>,
        exit_signal: Option<Signo>,
    ) -> Arc<Process> {
        let process = Self::prepare_with_task_number(leader_task_number, parent, exit_signal);
        let domain = process_domain::write_lock();
        process.attach_prepared_locked(&domain);
        process.publish_group_membership_locked(&domain);
        process
    }

    fn prepare_with_task_number(
        leader_task_number: Arc<PidHandle>,
        parent: Option<Arc<Process>>,
        exit_signal: Option<Signo>,
    ) -> Arc<Process> {
        let pid = leader_task_number.root_nr();
        let group = parent.as_ref().map_or_else(
            || {
                let session = Session::new(pid);
                ProcessGroup::new(pid, &session)
            },
            |p| p.group(),
        );
        let parent_relation = ParentRelation::prepare(pid, parent.as_ref(), exit_signal);
        let group_membership = GroupMembership::prepare(group.clone(), pid);

        Arc::new(Process {
            leader_task_number,
            exit_state: AtomicProcessExitState::new(ProcessExitState::Running),
            thread_membership: SpinNoIrq::new(ThreadMembership::default()),
            group_exit: SpinNoIrq::new(ThreadGroupExitState::default()),
            lifecycle: ProcessLifecycleState::new(),
            children: SpinNoIrq::new(List::new()),
            parent_relation,
            group_membership,
            runtime_ref: SpinNoIrq::new(None),
            pid_publication_slot: SpinNoIrq::new(None),
        })
    }

    fn attach_prepared_locked(
        self: &Arc<Self>,
        domain: &process_domain::ProcessDomainWriteGuard<'_>,
    ) {
        let (parent, exit_signal) = self.parent_relation.snapshot();

        if let Some(parent) = parent {
            self.attach_to_parent_slot_locked(domain, &parent, exit_signal);
        } else if INIT_PROC.get().is_none() {
            INIT_PROC.init_once(self.clone());
        }
    }

    /// Creates a init [`Process`].
    ///
    /// The first process created without a parent becomes the global init
    /// process returned by [`init_proc`].
    #[cfg(any(test, unittest))]
    pub fn new_init(pid: Pid) -> Arc<Process> {
        Self::new_init_with_task_number(PidHandle::fixed_root(pid))
    }

    /// Creates a child [`Process`].
    #[cfg(any(test, unittest))]
    pub fn fork(self: &Arc<Process>, pid: Pid) -> Arc<Process> {
        self.fork_with_exit_signal(pid, Some(Signo::SIGCHLD))
    }

    /// Creates a child [`Process`] with an explicit exit signal.
    #[cfg(any(test, unittest))]
    pub fn fork_with_exit_signal(
        self: &Arc<Process>,
        pid: Pid,
        exit_signal: Option<Signo>,
    ) -> Arc<Process> {
        self.fork_with_task_number(PidHandle::fixed_root(pid), exit_signal)
    }

    #[doc(hidden)]
    pub fn new_init_with_task_number(leader_task_number: Arc<PidHandle>) -> Arc<Process> {
        Self::new_with_task_number(leader_task_number, None, None)
    }

    #[doc(hidden)]
    pub fn fork_with_task_number(
        self: &Arc<Process>,
        leader_task_number: Arc<PidHandle>,
        exit_signal: Option<Signo>,
    ) -> Arc<Process> {
        Self::new_with_task_number(leader_task_number, Some(self.clone()), exit_signal)
    }
}

pub(crate) static INIT_PROC: LazyInit<Arc<Process>> = LazyInit::new();

/// Gets the init process.
///
/// This function panics if the init process has not been initialized yet.
pub fn init_proc() -> Arc<Process> {
    INIT_PROC.get().unwrap().clone()
}

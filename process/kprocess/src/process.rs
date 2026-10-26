// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process structure and lifecycle management.
mod exec_gate;
pub(crate) mod exit;
mod runtime_access;
mod thread_membership;
mod tree;

use alloc::sync::{Arc, Weak};
use core::{
    fmt,
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
};

use exit::{AtomicProcessExitState, ProcessExitState};
pub use exit::{
    ProcessExitPublication, WaitChildKind, WaitChildScan, WaitChildSelector, WaitReapMode,
    WaitedChild,
};
use kidentity::{PidHandle, PidNamespace};
use ksignal::Signo;
use kspin::SpinNoIrq;
use ksync::{Mutex, MutexGuard};
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

/// Stable process identity with parent/group relations, exit state and runtime capabilities.
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
    cgroup_target: Mutex<Option<Arc<kcgroup::Cgroup>>>,
    group_exit: SpinNoIrq<ThreadGroupExitState>,
    /// Process-wide `exec` serialization gate (Linux `cred_guard_mutex`).
    exec_gate: exec_gate::ExecGate,
    lifecycle: ProcessLifecycleState,
    /// Signal delivered to the parent's `wait` when this process's parent dies
    /// first (`prctl(PR_SET_PDEATHSIG)`).
    pdeath_signal: AtomicU32,

    /// Whether this thread group is a child subreaper (`PR_SET_CHILD_SUBREAPER`).
    ///
    /// Orphaned descendants reparent to the nearest subreaper ancestor instead
    /// of the child reaper, so runtimes such as containerd shims can observe
    /// and reap reparented container processes.
    child_subreaper: AtomicBool,
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
    /// This is the number used for kernel-internal lookup tables; the
    /// namespace-relative view that `getpid(2)` reports is
    /// [`Process::visible_pid`].
    pub fn pid(&self) -> Pid {
        self.leader_task_number.root_nr()
    }

    /// Returns the PID the leader reports through `getpid(2)`: its number in
    /// the PID namespace the process was created in (Linux
    /// `task_active_pid_ns`), so a `CLONE_NEWPID` child reads 1 there.
    /// `setns(CLONE_NEWPID)` retargets `pid_ns_for_children` and deliberately
    /// does not change this view.
    pub fn visible_pid(&self) -> Pid {
        self.leader_task_number.active_nr()
    }

    /// Returns the PID namespace the process lives in — its leader's deepest
    /// projection (Linux `task_active_pid_ns`). Every thread of the process
    /// reports this view through `getpid(2)`/`getppid(2)`, so the syscall
    /// boundary uses it as the observer namespace for PID arguments too.
    pub fn active_pid_namespace(&self) -> Arc<PidNamespace> {
        self.leader_task_number.active_ns().clone()
    }

    /// Returns the leader's exact number in `observer_ns`, or `None` when the
    /// identity carries no projection there — Linux `pid_nr_ns` semantics.
    /// This is the reporting-side inverse of allocation: the number a caller
    /// in `observer_ns` uses to name this process (wait results, clone
    /// return values).
    pub fn number_in_namespace(&self, observer_ns: &Arc<PidNamespace>) -> Option<Pid> {
        self.leader_task_number.number_in(observer_ns)
    }

    /// Returns the parent's PID as seen in this process's active PID
    /// namespace, or `None` when the parent has no number there — Linux
    /// `pid_nr_ns` semantics: a `CLONE_NEWPID` child's parent holds no
    /// projection in the child's namespace, so `getppid(2)` reads 0 there.
    /// Use [`Process::parent`] for the identity itself.
    pub fn visible_parent_pid(&self) -> Option<Pid> {
        let observer = self.leader_task_number.active_ns();
        let parent = self.parent()?;
        parent.leader_task_number.number_in(observer)
    }

    /// Translates a caller-visible PID into the root/global number the
    /// internal lookup tables are keyed on.
    ///
    /// Linux resolves `kill(2)`-style PID arguments in the caller's active PID
    /// namespace. The process table is root-keyed, so the syscall adapter
    /// translates first: a namespace number such as the `1` that
    /// `getpid(2)` reports inside a `CLONE_NEWPID` child maps back to the one
    /// process carrying that projection. Returns `None` when no published
    /// process matches — the caller turns that into `ESRCH`, matching Linux
    /// where a namespace-invisible PID simply does not exist.
    pub fn resolve_pid_in_namespace(nr: Pid, observer_ns: &Arc<PidNamespace>) -> Option<Pid> {
        crate::lookup::published_processes()
            .into_iter()
            .find(|proc| proc.leader_task_number.number_in(observer_ns) == Some(nr))
            .map(|proc| proc.pid())
    }

    /// Returns the exit signal configured for this process.
    pub fn exit_signal(&self) -> Option<Signo> {
        let _domain = process_domain::read_lock();
        self.parent_relation.exit_signal()
    }

    /// Returns whether this thread group is a child subreaper.
    pub fn is_child_subreaper(&self) -> bool {
        self.child_subreaper.load(Ordering::Relaxed)
    }

    /// Turns this thread group into (`true`) or out of (`false`) a child
    /// subreaper: orphaned descendants reparent to it instead of the child
    /// reaper.
    pub fn set_child_subreaper(&self, enabled: bool) {
        self.child_subreaper.store(enabled, Ordering::Relaxed)
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
        if self.is_exited_locked(domain) {
            self.release_exited_thread_members_locked(domain);
        }
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
            cgroup_target: Mutex::new(None),
            group_exit: SpinNoIrq::new(ThreadGroupExitState::default()),
            exec_gate: exec_gate::ExecGate::new(),
            pdeath_signal: AtomicU32::new(0),
            lifecycle: ProcessLifecycleState::new(),
            child_subreaper: AtomicBool::new(false),
            children: SpinNoIrq::new(List::new()),
            parent_relation,
            group_membership,
            runtime_ref: SpinNoIrq::new(None),
            pid_publication_slot: SpinNoIrq::new(None),
        })
    }

    /// Serializes thread publication and whole-process cgroup migration.
    pub(crate) fn cgroup_transaction(&self) -> MutexGuard<'_, Option<Arc<kcgroup::Cgroup>>> {
        self.cgroup_target.lock()
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

    /// Constructs a parentless identity using an already allocated leader handle.
    ///
    /// The first such process becomes global init. This does not construct a Thread
    /// or publish a runnable task; boot code must complete runtime setup first.
    #[doc(hidden)]
    pub fn new_init_with_task_number(leader_task_number: Arc<PidHandle>) -> Arc<Process> {
        Self::new_with_task_number(leader_task_number, None, None)
    }

    /// Constructs and attaches a child identity with its allocated leader handle.
    ///
    /// This low-level helper does not copy a runtime or activate a task. Prefer
    /// `Thread::prepare_process_fork` for complete fallible clone preparation.
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
///
/// # Panics
///
/// Panics when no initial process has been constructed.
pub fn init_proc() -> Arc<Process> {
    #[expect(
        clippy::unwrap_used,
        reason = "CG-021-no-panic-on-reachable-failure: init_proc requires the initial process to \
                  have been constructed, as documented in its contract"
    )]
    INIT_PROC.get().unwrap().clone()
}

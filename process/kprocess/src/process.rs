// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process structure and lifecycle management.
use alloc::{
    collections::BTreeMap,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    fmt,
    sync::atomic::{AtomicBool, Ordering},
};

use kcred::Credentials;
use kerrno::{KError, KResult};
use kfs::FsContext;
use kfutex::ProcessFutexState;
use khal::time::TimeValue;
use kidentity::PidHandle;
use kpoll::PollSet;
use ksignal::{
    Signo,
    api::{ProcessSignalManager, SignalActions},
};
use kspin::SpinNoIrq;
use ksync::{Mutex, spin::SpinNoIrq as KSyncSpinNoIrq};
use ktask::{KtaskRef, WeakKtaskRef};
use lazyinit::LazyInit;
use memspace::MmSpace;
use weak_map::StrongMap;

use crate::{
    AsThread, Pid, ProcessGroup, ProcessLifecycleState, ProcessRuntime, Session, Tid,
    publication::process_publication,
};

/// Thread group state tracked per process.
#[derive(Default)]
pub(crate) struct ThreadGroup {
    pub(crate) members: BTreeMap<Tid, WeakKtaskRef>,
    pub(crate) exit_code: i32,
    pub(crate) group_exited: bool,
}

/// A process.
pub struct Process {
    leader_task_number: Arc<PidHandle>,
    exit_signal: Option<Signo>,
    is_zombie: AtomicBool,
    pub(crate) thread_group: SpinNoIrq<ThreadGroup>,
    lifecycle: ProcessLifecycleState,

    // TODO: child subreaper9
    children: SpinNoIrq<StrongMap<Pid, Arc<Process>>>,
    parent: SpinNoIrq<Weak<Process>>,

    group: SpinNoIrq<Arc<ProcessGroup>>,
    runtime_ref: SpinNoIrq<Option<Weak<ProcessRuntime>>>,
}

/// Process-shared state updates that become visible after a successful exec.
pub struct ProcessExecUpdate {
    exe_path: String,
    cmdline: Arc<Vec<String>>,
    heap_top: usize,
    #[cfg(feature = "tee")]
    ta_head_bytes: Vec<u8>,
}

impl ProcessExecUpdate {
    /// Creates a process exec update with the new executable metadata and heap top.
    pub fn new(exe_path: String, cmdline: Arc<Vec<String>>, heap_top: usize) -> Self {
        Self {
            exe_path,
            cmdline,
            heap_top,
            #[cfg(feature = "tee")]
            ta_head_bytes: Vec::new(),
        }
    }

    /// Installs the TEE TA header bytes that should be applied on exec.
    #[cfg(feature = "tee")]
    pub fn with_ta_head_bytes(mut self, ta_head_bytes: Vec<u8>) -> Self {
        self.ta_head_bytes = ta_head_bytes;
        self
    }
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
        self.exit_signal
    }

    /// Returns `true` if the [`Process`] is the init process.
    ///
    /// This is a convenience method for checking if the [`Process`]
    /// [`Arc::ptr_eq`]s with the init process, which is cheaper than
    /// calling [`init_proc`] or testing if [`Process::parent`] is `None`.
    pub fn is_init(self: &Arc<Self>) -> bool {
        INIT_PROC.get().is_some_and(|init| Arc::ptr_eq(self, init))
    }

    /// Records a weak reference to the runtime owned by this process.
    pub(crate) fn set_runtime_ref(&self, runtime: &Arc<ProcessRuntime>) {
        *self.runtime_ref.lock() = Some(Arc::downgrade(runtime));
    }

    /// Returns the process runtime while a strong owner still exists.
    pub(crate) fn runtime_ref(&self) -> Option<Arc<ProcessRuntime>> {
        self.runtime_ref.lock().as_ref().and_then(Weak::upgrade)
    }

    fn runtime(&self) -> KResult<Arc<ProcessRuntime>> {
        self.runtime_ref().ok_or(KError::NoSuchProcess)
    }

    /// Returns the process-owned resource table while runtime remains attached.
    pub fn resources(&self) -> KResult<Arc<kresources::ProcessResources>> {
        self.runtime().map(|runtime| runtime.resources().clone())
    }

    /// Returns the filesystem context while runtime remains attached.
    pub fn fs_context(&self) -> KResult<Arc<Mutex<FsContext>>> {
        self.runtime().map(|runtime| runtime.fs_context())
    }

    /// Returns the process UTS namespace while runtime remains attached.
    pub fn uts_ns(&self) -> KResult<Arc<kns::UtsNamespace>> {
        self.runtime().map(|runtime| runtime.uts_ns())
    }

    /// Returns the address space while runtime remains attached.
    pub fn address_space(&self) -> KResult<Arc<Mutex<MmSpace>>> {
        self.runtime()
            .map(|runtime| runtime.address_space().clone())
    }

    /// Returns the signal manager while runtime remains attached.
    pub fn signal_manager(&self) -> KResult<Arc<ProcessSignalManager>> {
        self.runtime()
            .map(|runtime| runtime.signal_manager().clone())
    }

    /// Returns the shared signal actions while runtime remains attached.
    pub fn signal_actions(&self) -> KResult<Arc<KSyncSpinNoIrq<SignalActions>>> {
        self.signal_manager().map(|signal| signal.actions.clone())
    }

    /// Returns a snapshot of the current process credentials.
    pub fn credentials_snapshot(&self) -> KResult<Credentials> {
        self.runtime()
            .map(|runtime| runtime.with_credentials(Clone::clone))
    }

    /// Runs a closure with read-only access to process-shared credentials.
    pub fn with_credentials<R>(&self, f: impl FnOnce(&Credentials) -> R) -> KResult<R> {
        self.runtime().map(|runtime| runtime.with_credentials(f))
    }

    /// Runs a closure with mutable access to process-shared credentials.
    pub fn with_credentials_mut<R>(&self, f: impl FnOnce(&mut Credentials) -> R) -> KResult<R> {
        self.runtime()
            .map(|runtime| runtime.with_credentials_mut(f))
    }

    /// Returns the current executable path while runtime remains attached.
    pub fn exe_path(&self) -> KResult<String> {
        self.runtime()
            .map(|runtime| runtime.exe_path().read().clone())
    }

    /// Returns the current command line while runtime remains attached.
    pub fn cmdline(&self) -> KResult<Arc<Vec<String>>> {
        self.runtime()
            .map(|runtime| runtime.cmdline().read().clone())
    }

    /// Returns the current process umask while runtime remains attached.
    pub fn umask(&self) -> KResult<u32> {
        self.runtime().map(|runtime| runtime.umask())
    }

    /// Replaces the current process umask and returns the previous value.
    pub fn replace_umask(&self, umask: u32) -> KResult<u32> {
        self.runtime().map(|runtime| runtime.replace_umask(umask))
    }

    /// Returns the current process heap top while runtime remains attached.
    pub fn heap_top(&self) -> KResult<usize> {
        self.runtime().map(|runtime| runtime.heap_top())
    }

    /// Sets the current process heap top.
    pub fn set_heap_top(&self, top: usize) -> KResult<()> {
        self.runtime().map(|runtime| runtime.set_heap_top(top))
    }

    /// Returns the timer manager while runtime remains attached.
    pub fn timer_manager(&self) -> KResult<Arc<Mutex<ktimer::ProcessTimerManager>>> {
        self.runtime()
            .map(|runtime| runtime.timer_manager().clone())
    }

    /// Returns the futex state while runtime remains attached.
    pub fn futex_state(&self) -> KResult<Arc<ProcessFutexState>> {
        self.runtime().map(|runtime| runtime.futex_state().clone())
    }

    /// Updates the current executable metadata snapshot.
    pub fn set_exec_metadata(&self, exe_path: String, cmdline: Arc<Vec<String>>) -> KResult<()> {
        self.runtime()
            .map(|runtime| runtime.set_exec_metadata(exe_path, cmdline))
    }

    /// Applies the process-shared post-exec state transition.
    pub fn apply_exec_update(&self, update: ProcessExecUpdate) -> KResult<()> {
        self.set_exec_metadata(update.exe_path.clone(), update.cmdline.clone())?;
        self.apply_exec_tee_update(&update)?;
        self.set_heap_top(update.heap_top)?;
        self.with_credentials_mut(|credentials| credentials.apply_exec())?;
        self.reset_signal_actions()?;
        self.clear_posix_timers()?;
        self.close_cloexec_files()?;
        #[cfg(feature = "tee")]
        self.clear_tee_runtime_private()?;
        Ok(())
    }

    #[cfg(feature = "tee_ta_sign")]
    fn apply_exec_tee_update(&self, update: &ProcessExecUpdate) -> KResult<()> {
        self.with_tee_ta_ctx_mut(|tee_ta_ctx| {
            tee_ta_ctx.init_ta_ctx(update.exe_path.as_str(), update.ta_head_bytes.as_slice());
        })
    }

    #[cfg(not(feature = "tee_ta_sign"))]
    fn apply_exec_tee_update(&self, _update: &ProcessExecUpdate) -> KResult<()> {
        Ok(())
    }

    /// Resets process-wide signal actions to defaults.
    pub fn reset_signal_actions(&self) -> KResult<()> {
        self.runtime().map(|runtime| runtime.reset_signal_actions())
    }

    /// Clears all process POSIX timers.
    pub fn clear_posix_timers(&self) -> KResult<()> {
        self.runtime().map(|runtime| runtime.clear_posix_timers())
    }

    /// Closes all process file descriptors.
    pub fn close_all_fds(&self) -> KResult<()> {
        self.runtime()
            .map(|runtime| runtime.resources().close_all_fds())
    }

    /// Closes all process file descriptors marked `FD_CLOEXEC`.
    pub fn close_cloexec_files(&self) -> KResult<()> {
        self.runtime()
            .map(|runtime| runtime.resources().close_cloexec_files())
    }

    /// Clears process-local TEE runtime private state.
    #[cfg(feature = "tee")]
    pub fn clear_tee_runtime_private(&self) -> KResult<()> {
        self.runtime()
            .map(|runtime| runtime.clear_tee_runtime_private())
    }

    /// Runs a closure with immutable access to the process-shared TEE TA context.
    #[cfg(feature = "tee")]
    pub fn with_tee_ta_ctx<R>(&self, f: impl FnOnce(&tee_task_iface::TeeTaCtx) -> R) -> KResult<R> {
        self.runtime().map(|runtime| runtime.with_tee_ta_ctx(f))
    }

    /// Runs a closure with mutable access to the process-shared TEE TA context.
    #[cfg(feature = "tee")]
    pub fn with_tee_ta_ctx_mut<R>(
        &self,
        f: impl FnOnce(&mut tee_task_iface::TeeTaCtx) -> R,
    ) -> KResult<R> {
        self.runtime().map(|runtime| runtime.with_tee_ta_ctx_mut(f))
    }

    /// Returns sampled user and system CPU time in nanoseconds.
    pub fn process_cpu_time_ns(&self) -> (usize, usize) {
        let (utime_ns, stime_ns) = self.exited_thread_time_ns();
        self.thread_tasks()
            .into_iter()
            .fold((utime_ns, stime_ns), |(utime_ns, stime_ns), task| {
                let (thread_utime_ns, thread_stime_ns) = task.as_thread().sample_cpu_time_ns();
                (
                    utime_ns.saturating_add(thread_utime_ns),
                    stime_ns.saturating_add(thread_stime_ns),
                )
            })
    }

    /// Returns sampled user and system CPU time.
    pub fn process_cpu_times(&self) -> (TimeValue, TimeValue) {
        let (utime_ns, stime_ns) = self.process_cpu_time_ns();
        (
            TimeValue::from_nanos(utime_ns as u64),
            TimeValue::from_nanos(stime_ns as u64),
        )
    }

    /// Returns the total sampled CPU time.
    pub fn process_cpu_time(&self) -> TimeValue {
        let (utime, stime) = self.process_cpu_times();
        utime + stime
    }
}

/// Parent & children
impl Process {
    /// The parent [`Process`].
    pub fn parent(&self) -> Option<Arc<Process>> {
        self.parent.lock().upgrade()
    }

    /// The child [`Process`]es.
    pub fn children(&self) -> Vec<Arc<Process>> {
        self.children.lock().values().cloned().collect()
    }
}

/// [`ProcessGroup`] & [`Session`]
impl Process {
    /// The [`ProcessGroup`] that the [`Process`] belongs to.
    pub fn group(&self) -> Arc<ProcessGroup> {
        self.group.lock().clone()
    }

    fn set_group(self: &Arc<Self>, group: &Arc<ProcessGroup>) {
        let mut self_group = self.group.lock();
        if Arc::ptr_eq(&self_group, group) {
            return;
        }

        let current_group = self_group.clone();
        let current_ptr = Arc::as_ptr(&current_group).addr();
        let target_ptr = Arc::as_ptr(group).addr();

        if current_ptr < target_ptr {
            let mut current_members = current_group.processes.lock();
            let mut target_members = group.processes.lock();
            let pid = self.pid();
            current_members.remove(&pid);
            target_members.insert(pid, self);
        } else {
            let mut target_members = group.processes.lock();
            let mut current_members = current_group.processes.lock();
            let pid = self.pid();
            current_members.remove(&pid);
            target_members.insert(pid, self);
        }

        *self_group = group.clone();
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
        if self.group.lock().session.sid() == pid {
            return None;
        }

        let new_session = Session::new(pid);
        let new_group = ProcessGroup::new(pid, &new_session);
        self.set_group(&new_group);
        process_publication().refresh_job_control_identity(self);

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
        if self.group.lock().pgid() == pid {
            return None;
        }

        let new_group = ProcessGroup::new(pid, &self.group.lock().session);
        self.set_group(&new_group);
        process_publication().refresh_job_control_identity(self);

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
        if Arc::ptr_eq(&self.group.lock(), group) {
            return true;
        }

        if !Arc::ptr_eq(&self.group.lock().session, &group.session) {
            return false;
        }

        self.set_group(group);
        true
    }
}

/// Threads
impl Process {
    fn prune_stale_thread_members(thread_group: &mut ThreadGroup) {
        thread_group
            .members
            .retain(|_, weak_task| weak_task.upgrade().is_some());
    }

    /// Publishes a thread task into this [`Process`]'s membership table.
    pub(crate) fn add_thread_task(self: &Arc<Self>, task: &KtaskRef) {
        self.thread_group
            .lock()
            .members
            .insert(task.as_thread().tid(), Arc::downgrade(task));
    }

    /// Removes a previously published thread task from this [`Process`]'s membership table.
    pub(crate) fn remove_thread_task(&self, tid: Tid) {
        self.thread_group.lock().members.remove(&tid);
    }

    /// Removes a thread from this [`Process`] and sets the exit code if the
    /// group has not exited.
    ///
    /// Returns `true` if this was the last thread in the process.
    pub(crate) fn exit_thread(self: &Arc<Self>, tid: Tid, exit_code: i32) -> bool {
        let mut thread_group = self.thread_group.lock();
        if !thread_group.group_exited {
            thread_group.exit_code = exit_code;
        }
        thread_group.members.remove(&tid);
        Self::prune_stale_thread_members(&mut thread_group);
        thread_group.members.is_empty()
    }

    /// Returns a snapshot of published thread IDs in this [`Process`].
    pub(crate) fn threads(&self) -> Vec<Tid> {
        let thread_group = self.thread_group.lock();
        thread_group
            .members
            .iter()
            .filter_map(|(tid, weak_task)| weak_task.upgrade().map(|_| *tid))
            .collect()
    }

    /// Returns a snapshot of published thread tasks in this [`Process`].
    pub(crate) fn thread_tasks(&self) -> Vec<KtaskRef> {
        let thread_group = self.thread_group.lock();
        thread_group
            .members
            .values()
            .filter_map(|weak_task| weak_task.upgrade())
            .collect()
    }

    /// Returns the number of published threads currently attached to this [`Process`].
    pub(crate) fn thread_count(&self) -> usize {
        let thread_group = self.thread_group.lock();
        thread_group
            .members
            .values()
            .filter(|weak_task| weak_task.upgrade().is_some())
            .count()
    }

    /// Returns whether `tid` still resolves to a published thread in this [`Process`].
    pub(crate) fn contains_published_tid(&self, tid: Tid) -> bool {
        let thread_group = self.thread_group.lock();
        thread_group
            .members
            .get(&tid)
            .is_some_and(|weak_task| weak_task.upgrade().is_some())
    }

    /// Returns the lowest-TID published task that currently represents this [`Process`].
    pub(crate) fn representative_task(&self) -> KResult<KtaskRef> {
        let thread_group = self.thread_group.lock();
        thread_group
            .members
            .values()
            .find_map(|weak_task| weak_task.upgrade())
            .ok_or(KError::NoSuchProcess)
    }

    /// Returns `true` if the [`Process`] is group exited.
    pub fn is_group_exited(&self) -> bool {
        self.thread_group.lock().group_exited
    }

    /// Marks the [`Process`] as group exited.
    pub(crate) fn group_exit(&self) {
        self.thread_group.lock().group_exited = true;
    }

    /// The exit code of the [`Process`].
    pub fn exit_code(&self) -> i32 {
        self.thread_group.lock().exit_code
    }
}

/// Status & exit
impl Process {
    /// Returns `true` if the [`Process`] is a zombie process.
    pub fn is_zombie(&self) -> bool {
        self.is_zombie.load(Ordering::Acquire)
    }

    /// Returns the child-exit wait event for this process.
    pub fn child_exit_event(&self) -> &Arc<PollSet> {
        self.lifecycle.child_exit_event()
    }

    /// Returns the process-exit event for this process.
    pub fn exit_event(&self) -> &Arc<PollSet> {
        self.lifecycle.exit_event()
    }

    /// Adds exited-thread CPU time to this process's accumulated counters.
    pub(crate) fn accumulate_exited_thread_time(&self, utime_ns: usize, stime_ns: usize) {
        self.lifecycle
            .accumulate_exited_thread_time(utime_ns, stime_ns);
    }

    /// Returns accumulated exited-thread user and kernel time in nanoseconds.
    pub fn exited_thread_time_ns(&self) -> (usize, usize) {
        self.lifecycle.exited_thread_time_ns()
    }

    /// Adds reaped-child CPU time to this process's accumulated counters.
    pub(crate) fn accumulate_child_time(&self, utime_ns: usize, stime_ns: usize) {
        self.lifecycle.accumulate_child_time(utime_ns, stime_ns);
    }

    /// Returns accumulated reaped-children user and kernel time in nanoseconds.
    pub fn child_time_ns(&self) -> (usize, usize) {
        self.lifecycle.child_time_ns()
    }

    /// Terminates the [`Process`], marking it as a zombie process.
    ///
    /// Child processes are inherited by the init process or by the nearest
    /// subreaper process.
    ///
    /// If the [`Process`] is the init process, this method returns without
    /// changing its state.
    pub(crate) fn exit(self: &Arc<Self>) {
        // TODO: child subreaper
        let reaper = INIT_PROC.get().unwrap();

        if Arc::ptr_eq(self, reaper) {
            return;
        }

        // Lock order: reaper.children → self.children.
        // init never exits (returns early above), so it never acquires its
        // own children lock in the reverse order.  All other exit() callers
        // follow the same "reaper first" rule, preventing AB-BA deadlocks.
        // This also avoids any window where orphans are invisible to waitpid.
        let mut reaper_children = reaper.children.lock();
        let mut children = self.children.lock();
        self.is_zombie.store(true, Ordering::Release);

        let reaper_weak = Arc::downgrade(reaper);
        for (pid, child) in core::mem::take(&mut *children) {
            *child.parent.lock() = reaper_weak.clone();
            reaper_children.insert(pid, child);
        }
    }

    /// Frees a zombie [`Process`]. Removes it from the parent.
    ///
    /// This method panics if the [`Process`] is not a zombie.
    pub(crate) fn free(&self) {
        assert!(self.is_zombie(), "only zombie process can be freed");

        if let Some(parent) = self.parent() {
            parent.children.lock().remove(&self.pid());
        }
    }

    /// Discards a process that never completed external publication.
    pub(crate) fn discard_unpublished(&self) {
        if let Some(parent) = self.parent() {
            parent.children.lock().remove(&self.pid());
        }
        self.group().processes.lock().remove(&self.pid());
    }

    /// Removes this zombie process from its parent's live child relation once.
    ///
    /// Returns `true` only for the first successful reaper. Subsequent callers
    /// observing the same zombie through stale references will see `false`.
    pub(crate) fn try_detach_from_parent(&self) -> bool {
        if !self.is_zombie() {
            return false;
        }

        let Some(parent) = self.parent() else {
            return false;
        };

        parent.children.lock().remove(&self.pid()).is_some()
    }
}

impl fmt::Debug for Process {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut builder = f.debug_struct("Process");
        builder.field("pid", &self.pid());

        let thread_group = self.thread_group.lock();
        if thread_group.group_exited {
            builder.field("group_exited", &thread_group.group_exited);
        }
        if self.is_zombie() {
            builder.field("exit_code", &thread_group.exit_code);
        }

        if let Some(parent) = self.parent() {
            builder.field("parent", &parent.pid());
        }
        builder.field("group", &self.group());
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
        let pid = leader_task_number.root_nr();
        let group = parent.as_ref().map_or_else(
            || {
                let session = Session::new(pid);
                ProcessGroup::new(pid, &session)
            },
            |p| p.group(),
        );

        let process = Arc::new(Process {
            leader_task_number,
            exit_signal,
            is_zombie: AtomicBool::new(false),
            thread_group: SpinNoIrq::new(ThreadGroup::default()),
            lifecycle: ProcessLifecycleState::new(),
            children: SpinNoIrq::new(StrongMap::new()),
            parent: SpinNoIrq::new(parent.as_ref().map(Arc::downgrade).unwrap_or_default()),
            group: SpinNoIrq::new(group.clone()),
            runtime_ref: SpinNoIrq::new(None),
        });

        group.processes.lock().insert(pid, &process);

        if let Some(parent) = parent {
            parent.children.lock().insert(pid, process.clone());
        } else if INIT_PROC.get().is_none() {
            INIT_PROC.init_once(process.clone());
        }

        process
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

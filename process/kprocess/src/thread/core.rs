// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicUsize, Ordering};

use kalloc_types::{AllocFlags, KBox};
use kcgroup::{Cgroup, TaskCharge, TaskMembership};
use kcred::Cred;
use kerrno::KResult;
use ksignal::{SignalStack, api::ThreadSignalManager};
use kspin::SpinNoIrq;
use ksync::{Mutex, RwLock};
use ktask::KtaskRef;
use ktime_types::TimeSpan;
#[cfg(feature = "tee")]
use tee_task_iface::TeeSessionCtxTrait;

use super::{
    cpu_time::{PrevCputime, ThreadCpuTime},
    restart::RestartSlot,
};
use crate::{
    Pid, Process, ProcessForkConfig, ProcessRuntime, Tid, allocate_thread_task_number,
    fork_process_runtime, process_runtime::ExitAddressSpace,
};

/// The current user thread handle.
pub struct CurrentThread(pub(super) KtaskRef);

/// Fully prepared user-thread clone/fork result.
///
/// This bundles the preallocated thread identity together with the matching
/// thread object so higher layers can construct the target task without
/// re-implementing PID namespace or identity-allocation policy.
pub struct PreparedUserClone {
    thread: KBox<Thread>,
    task_number: Arc<kidentity::PidHandle>,
}

impl PreparedUserClone {
    /// Returns the user-visible thread identifier.
    pub fn tid(&self) -> Tid {
        self.thread.tid()
    }

    /// Returns the process identity that owns the prepared thread.
    pub fn process(&self) -> &Arc<Process> {
        self.thread.process()
    }

    /// Returns the page-table root that the target task should install before
    /// first entering user space.
    ///
    /// # Panics
    ///
    /// Panics when the prepared thread has lost its process runtime or live
    /// address-space owner.
    pub fn page_table_root(&self) -> karch::HwPageTableRoot {
        self.thread
            .process()
            .address_space()
            .expect("prepared user clone must have a live process address space")
            .lock()
            .page_table_hw_root()
    }

    /// Splits the prepared clone into the thread object and its task identity.
    pub fn into_parts(self) -> (KBox<Thread>, Arc<kidentity::PidHandle>) {
        (self.thread, self.task_number)
    }
}

/// Linux nice value.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct NiceValue(i32);

impl NiceValue {
    /// Default Linux nice value.
    pub const DEFAULT: Self = Self(0);
    /// Highest Linux nice value.
    pub const MAX: Self = Self(Self::MAX_RAW);
    /// Highest raw Linux nice value.
    pub const MAX_RAW: i32 = 19;
    /// Lowest Linux nice value.
    pub const MIN: Self = Self(Self::MIN_RAW);
    /// Lowest raw Linux nice value.
    pub const MIN_RAW: i32 = -20;

    /// Returns `value` clamped to the Linux nice range.
    pub fn new_clamped(value: i32) -> Self {
        Self(value.clamp(Self::MIN_RAW, Self::MAX_RAW))
    }

    /// Returns the raw Linux nice value.
    pub fn as_i32(self) -> i32 {
        self.0
    }

    /// Returns the priority value exported by `/proc/[pid]/stat`.
    pub fn proc_stat_priority(self) -> i32 {
        20 + self.0
    }

    /// Returns the raw value exported by `getpriority(2)`.
    pub fn getpriority_raw(self) -> isize {
        (20 - self.0) as isize
    }

    /// Returns the scheduler priority value used by fair schedulers.
    pub fn fair_scheduler_priority(self) -> isize {
        self.0 as isize
    }
}

impl Default for NiceValue {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Consistent scheduler policy/priority snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SchedulerParameters {
    policy: Option<u32>,
    priority: i32,
}

impl SchedulerParameters {
    fn new(policy: Option<u32>, priority: i32) -> Self {
        Self { policy, priority }
    }

    /// Returns the explicit scheduler policy, if one was configured.
    pub fn policy(self) -> Option<u32> {
        self.policy
    }

    /// Returns the configured scheduler priority.
    pub fn priority(self) -> i32 {
        self.priority
    }
}

#[derive(Debug, Default)]
struct SchedulerState {
    policy: Option<u32>,
    priority: i32,
}

impl SchedulerState {
    fn parameters(&self) -> SchedulerParameters {
        SchedulerParameters::new(self.policy, self.priority)
    }
}

/// Task-bound user-thread state, credentials, signals and shared process runtime.
pub struct Thread {
    /// Stable identity token, allocated once and unchanged across an `exec`
    /// leader takeover. Registries that must not be re-indexed when the number
    /// moves (thread membership, cgroup membership) key on this token.
    identity_token: kidentity::TaskIdentityToken,
    /// Current number binding for this thread. It is stable for the task's life
    /// except for an `exec` leader takeover, which moves the thread-group
    /// number onto the surviving thread; registries key on the task object, not
    /// on this number.
    task_number: SpinNoIrq<Arc<kidentity::PidHandle>>,
    /// Lock-free snapshot of the current numeric TID, kept in step with
    /// `task_number` by the same critical section that moves it.
    tid: AtomicU32,
    process: Arc<Process>,
    runtime: Arc<ProcessRuntime>,
    files: kresources::TaskFiles,
    // Installed once before membership retirement; released only with Thread.
    exit_address_space: SpinNoIrq<Option<ExitAddressSpace>>,
    real_cred: RwLock<Arc<Cred>>,
    cred: RwLock<Arc<Cred>>,
    clear_child_tid: AtomicUsize,
    robust_list_head: AtomicUsize,
    signal: Arc<ThreadSignalManager>,
    time: ThreadCpuTime,
    /// Monotonic adjustment history for this thread's user/system split.
    prev_cputime: PrevCputime,
    /// Coalesces periodic CPU-timer work requests until this task returns to
    /// userspace.
    ///
    /// Publication relationship covered by every ordering on this bit:
    /// producer is the local timer tick ([`Thread::request_cpu_timer_work`]),
    /// whose `Release` store publishes the relaxed raw tick additions the
    /// same tick just performed ([`ThreadCpuTime`]) and the armed-gate
    /// value it read. Observers are the user-return probe
    /// ([`Thread::has_pending_cpu_timer_work`], `Acquire` load) and the
    /// consumer in the return tail ([`Thread::take_cpu_timer_work`], `AcqRel`
    /// swap). Their `Acquire` side pairs with the tick's `Release`, so a poll
    /// runs on samples at least as recent as the tick that requested it. The
    /// swap is a single `AcqRel` RMW: it consumes exactly one coalesced
    /// request even when a tick on the way to another return re-arms the bit
    /// concurrently, and its `Release` side orders that re-request after the
    /// consumption instead of letting it be lost to a stale `false`.
    cpu_timer_work_pending: AtomicBool,
    nice: AtomicI32,
    scheduler: Mutex<SchedulerState>,
    is_exiting: AtomicBool,
    /// Set while this thread is a `de_thread` sibling-sweep target.
    ///
    /// Its fatal-signal death must not mark the whole group exited or broadcast
    /// a group kill; the execing thread orchestrates the sweep and must survive
    /// it. See `exec_de_thread`.
    exec_sweep_target: AtomicBool,
    accessing_user_memory: AtomicBool,
    pub(super) restart_block: Mutex<RestartSlot>,
    no_new_privileges: AtomicBool,
    seccomp: super::SeccompContext,
    cgroup: TaskMembership,
    #[cfg(feature = "tee")]
    tee_session_ctx: Mutex<Option<KBox<dyn TeeSessionCtxTrait>>>,
}

impl Thread {
    /// Returns this task's descriptor-table binding.
    pub fn files(&self) -> &kresources::TaskFiles {
        &self.files
    }

    /// Applies the caller's files transition and process-shared post-exec update.
    ///
    /// Unsharing first keeps close-on-exec from changing another task's files
    /// or closing that table's descriptors. Call after retiring sibling threads.
    ///
    /// # Errors
    /// Forwards detached-files and process-update errors. The caller must treat
    /// failure after image commit as fatal; this operation does not roll back.
    pub fn apply_exec_update(&self, update: crate::ProcessExecUpdate) -> KResult<()> {
        self.files.unshare_fd_table()?;
        self.files.close_cloexec_files()?;
        self.process.apply_exec_update(update)
    }

    /// Creates a thread in sleepable, reclaim-compatible task context.
    pub(crate) fn new(
        process: Arc<Process>,
        runtime: Arc<ProcessRuntime>,
        task_number: Arc<kidentity::PidHandle>,
        cred: Arc<Cred>,
    ) -> KResult<KBox<Self>> {
        let charge = runtime
            .nsproxy()
            .expect("initial process runtime must have a namespace owner")
            .cgroup_ns()
            .root()
            .reserve_task()?;
        Self::new_with_charge(
            process,
            runtime,
            task_number,
            cred,
            charge,
            false,
            kresources::TaskFiles::new_shared(),
            AllocFlags::KERNEL,
        )
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "CG-024-lint-exceptions: clippy::too_many_arguments: Thread construction binds \
                  process identity, resource ownership and explicit allocation permissions"
    )]
    fn new_with_charge(
        process: Arc<Process>,
        runtime: Arc<ProcessRuntime>,
        task_number: Arc<kidentity::PidHandle>,
        cred: Arc<Cred>,
        charge: TaskCharge,
        no_new_privileges: bool,
        files: kresources::TaskFiles,
        flags: AllocFlags,
    ) -> KResult<KBox<Self>> {
        let tid = task_number.root_nr();
        let identity_token = kidentity::TaskIdentityToken::allocate();
        let cgroup = charge.commit(identity_token)?;
        KBox::new_with_flags(
            Thread {
                identity_token,
                task_number: SpinNoIrq::new(task_number),
                tid: AtomicU32::new(tid),
                signal: ThreadSignalManager::new_with_flags(
                    tid,
                    runtime.signal_manager().clone(),
                    flags,
                )?,
                process,
                runtime,
                files,
                exit_address_space: SpinNoIrq::new(None),
                real_cred: RwLock::new(cred.clone()),
                cred: RwLock::new(cred),
                clear_child_tid: AtomicUsize::new(0),
                robust_list_head: AtomicUsize::new(0),
                time: ThreadCpuTime::new(),
                prev_cputime: PrevCputime::new(),
                cpu_timer_work_pending: AtomicBool::new(false),
                nice: AtomicI32::new(NiceValue::default().as_i32()),
                scheduler: Mutex::new(SchedulerState::default()),
                is_exiting: AtomicBool::new(false),
                exec_sweep_target: AtomicBool::new(false),
                accessing_user_memory: AtomicBool::new(false),
                restart_block: Mutex::new(RestartSlot::empty()),
                no_new_privileges: AtomicBool::new(no_new_privileges),
                seccomp: super::SeccompContext::new(),
                cgroup,
                #[cfg(feature = "tee")]
                tee_session_ctx: Mutex::new(None),
            },
            flags,
        )
    }

    /// Forks a child process from this thread's current process and returns the
    /// child thread object for installation into a new task.
    ///
    /// # Errors
    ///
    /// Forwards `Thread::prepare_process_fork` errors; no task is activated.
    pub fn fork_process_child(&self, config: ProcessForkConfig) -> KResult<KBox<Self>> {
        self.prepare_process_fork(config)
            .map(|prepared| prepared.into_parts().0)
    }

    /// Prepares a child process clone together with its leader task identity.
    /// Copies this thread's blocked signal mask before publication; the child's
    /// private and process-wide pending queues start empty even with shared actions.
    ///
    /// When `config.cgroup_target` is set (`clone3(CLONE_INTO_CGROUP)`), the
    /// child's membership reservation is charged against that node instead of
    /// this thread's cgroup; the caller must already have authorized the target.
    ///
    /// # Errors
    ///
    /// Forwards cgroup reservation/commit, identity allocation, namespace, MM and
    /// fd-table preparation errors. Runtime fork returns NoSuchProcess for detached
    /// owners, WouldBlock for shared fs during exec, InvalidInput for invalid or
    /// changed parent/namespace contracts, and Unsupported for an unimplemented
    /// namespace request. Thread and signal-slot allocation use NOWAIT under the cgroup
    /// transaction mutex and may return NoMemory. The prepared child is not activated.
    pub fn prepare_process_fork(
        &self,
        mut config: ProcessForkConfig,
    ) -> KResult<PreparedUserClone> {
        let _transaction = self.process.cgroup_transaction();
        let charge = match config.cgroup_target.take() {
            Some(target) => target.reserve_task()?,
            None => self.cgroup.reserve_child()?,
        };
        let files = self
            .files
            .fork(matches!(config.fd_table, crate::ForkFdTable::Shared))?;
        let parent_cred = self.subjective_cred();
        fork_process_runtime(
            &self.process_runtime(),
            config,
            self.subjective_cred(),
            |process_runtime, task_number, user_ns| {
                let cred = match user_ns {
                    Some(user_ns) => Arc::new(parent_cred.with_user_namespace(user_ns)),
                    None => parent_cred,
                };
                let thread = Self::new_with_charge(
                    process_runtime.process().clone(),
                    process_runtime,
                    task_number.clone(),
                    cred,
                    charge,
                    self.no_new_privileges(),
                    files,
                    AllocFlags::NOWAIT,
                )?;
                thread.signal.set_blocked(self.signal.blocked());
                thread.seccomp.inherit_from(&self.seccomp);
                Ok(PreparedUserClone {
                    thread,
                    task_number,
                })
            },
        )
    }

    /// Creates a sibling thread within the same process runtime.
    ///
    /// # Errors
    ///
    /// Forwards `Thread::prepare_thread_clone` errors; no task is activated.
    pub fn clone_thread_in_process(&self) -> KResult<KBox<Self>> {
        self.prepare_thread_clone()
            .map(|prepared| prepared.into_parts().0)
    }

    /// Prepares a sibling thread clone together with its thread identity.
    /// Copies this thread's blocked signal mask before publication. Private
    /// pending starts empty; the existing process-wide pending queue stays shared.
    ///
    /// # Errors
    ///
    /// Forwards cgroup reservation/commit and root thread-identity allocation
    /// errors, including NoMemory for NOWAIT thread or signal-slot allocation under the
    /// cgroup transaction mutex. The sibling shares its runtime and is not activated.
    pub fn prepare_thread_clone(&self) -> KResult<PreparedUserClone> {
        self.prepare_thread_clone_with_files(true)
    }

    /// Prepares a sibling, sharing the table only when `CLONE_FILES` is set.
    ///
    /// # Errors
    /// Forwards clone preparation and detached-files errors.
    pub fn prepare_thread_clone_with_files(&self, is_shared: bool) -> KResult<PreparedUserClone> {
        let _transaction = self.process.cgroup_transaction();
        let charge = self.cgroup.reserve_child()?;
        let task_number = allocate_thread_task_number(&self.process.active_pid_namespace())?;
        let thread = Self::new_with_charge(
            self.process.clone(),
            self.runtime.clone(),
            task_number.clone(),
            self.subjective_cred(),
            charge,
            self.no_new_privileges(),
            self.files.fork(is_shared)?,
            AllocFlags::NOWAIT,
        )?;
        thread.signal.set_blocked(self.signal.blocked());
        thread.seccomp.inherit_from(&self.seccomp);
        Ok(PreparedUserClone {
            thread,
            task_number,
        })
    }

    /// Returns the user-visible thread identifier.
    pub fn tid(&self) -> Tid {
        self.tid.load(Ordering::Relaxed)
    }

    /// Returns the task's canonical cgroup node.
    pub fn cgroup(&self) -> Option<Arc<Cgroup>> {
        self.cgroup.cgroup()
    }

    /// Moves this task to an existing cgroup.
    ///
    /// # Errors
    ///
    /// Forwards `kcgroup::TaskMembership::migrate` errors for the requested group.
    pub fn migrate_cgroup(&self, target: &Arc<Cgroup>) -> KResult<()> {
        self.cgroup.migrate(target)
    }

    /// Detaches this exiting thread from its cgroup.
    pub fn detach_cgroup(&self) {
        self.cgroup.detach();
    }

    /// Returns whether privilege-gaining exec transitions are disabled.
    pub fn no_new_privileges(&self) -> bool {
        self.no_new_privileges.load(Ordering::Acquire)
    }

    /// Permanently disables privilege-gaining exec transitions for this task.
    pub fn set_no_new_privileges(&self) {
        self.no_new_privileges.store(true, Ordering::Release);
    }

    pub(crate) fn cgroup_membership(&self) -> &TaskMembership {
        &self.cgroup
    }

    /// Returns the thread-local seccomp filter state.
    pub fn seccomp(&self) -> &super::SeccompContext {
        &self.seccomp
    }

    /// Returns the `clear_child_tid` user pointer for this thread.
    pub fn clear_child_tid(&self) -> usize {
        self.clear_child_tid.load(Ordering::Relaxed)
    }

    /// Sets the `clear_child_tid` user pointer for this thread.
    pub fn set_clear_child_tid(&self, clear_child_tid: usize) {
        self.clear_child_tid
            .store(clear_child_tid, Ordering::Relaxed);
    }

    /// Resets thread-private exec state that still points into the old user image.
    ///
    /// Clears clear_child_tid, the robust-list head and alternate signal stack.
    /// Call immediately after successful MM exchange, before fallible new-image work.
    pub fn reset_user_state_after_exec(&self) {
        self.set_clear_child_tid(0);
        self.set_robust_list_head(0);
        self.clear_restart_block();
        self.signal.set_stack(SignalStack::default());
    }

    /// Releases the old TEE session at the successful exec tail after credential commit.
    ///
    /// Takes the slot, releases its mutex, then drops the session. This does not
    /// perform the active close-all cleanup used by fatal exit.
    #[cfg(feature = "tee")]
    pub fn reset_tee_session_after_exec(&self) {
        let session = self.tee_session_ctx.lock().take();
        drop(session);
    }

    /// Returns the robust-futex list head pointer registered by this thread.
    pub fn robust_list_head(&self) -> usize {
        self.robust_list_head.load(Ordering::Acquire)
    }

    /// Sets the robust-futex list head pointer for this thread.
    pub fn set_robust_list_head(&self, robust_list_head: usize) {
        self.robust_list_head
            .store(robust_list_head, Ordering::Release);
    }

    /// Returns the process-shared OOM score adjustment.
    pub fn oom_score_adj(&self) -> i32 {
        self.runtime.oom_score_adj()
    }

    /// Sets the process-shared OOM score adjustment.
    pub fn set_oom_score_adj(&self, value: i32) {
        self.runtime.set_oom_score_adj(value);
    }

    /// Returns the thread's scheduler nice value.
    pub fn nice(&self) -> NiceValue {
        NiceValue::new_clamped(self.nice.load(Ordering::Acquire))
    }

    /// Sets the thread's scheduler nice value.
    pub fn set_nice(&self, nice: NiceValue) {
        self.nice.store(nice.as_i32(), Ordering::Release);
    }

    /// Returns the thread's scheduler policy/priority snapshot.
    pub fn scheduler_parameters(&self) -> SchedulerParameters {
        self.scheduler.lock().parameters()
    }

    /// Returns the explicit scheduler policy if one has been configured.
    pub fn scheduler_policy(&self) -> Option<u32> {
        self.scheduler_parameters().policy()
    }

    /// Returns the configured scheduler priority.
    pub fn scheduler_priority(&self) -> i32 {
        self.scheduler_parameters().priority()
    }

    /// Sets the scheduler policy and priority for this thread.
    pub fn set_scheduler(&self, policy: u32, priority: i32) {
        let mut scheduler = self.scheduler.lock();
        scheduler.policy = Some(policy);
        scheduler.priority = priority;
    }

    /// Updates the scheduler priority after validating it against the current
    /// effective policy while holding the scheduler state lock.
    ///
    /// # Errors
    ///
    /// Returns the validation callback error without changing priority. The
    /// callback runs under the scheduler state lock and must not reenter it.
    pub fn set_scheduler_priority_with<F>(
        &self,
        priority: i32,
        default_policy: u32,
        validate: F,
    ) -> KResult<()>
    where
        F: FnOnce(u32, i32) -> KResult<()>,
    {
        let mut scheduler = self.scheduler.lock();
        let policy = scheduler.policy.unwrap_or(default_policy);
        validate(policy, priority)?;
        scheduler.priority = priority;
        Ok(())
    }

    /// Returns whether the thread is in its exit path.
    pub fn is_exiting(&self) -> bool {
        self.is_exiting.load(Ordering::Acquire)
    }

    /// Marks the thread as exiting.
    pub fn set_exit(&self) {
        self.is_exiting.store(true, Ordering::Release);
    }

    /// Marks this thread as a `de_thread` sibling-sweep target.
    ///
    /// A sweep target's fatal death skips the group-exit marking and group-kill
    /// broadcast in `do_exit`, so the execing thread survives the sweep.
    pub fn mark_exec_sweep_target(&self) {
        self.exec_sweep_target.store(true, Ordering::Release);
    }

    /// Returns whether this thread is a `de_thread` sibling-sweep target.
    pub fn is_exec_sweep_target(&self) -> bool {
        self.exec_sweep_target.load(Ordering::Acquire)
    }

    /// Returns whether the thread is currently performing a user-memory access.
    pub fn is_accessing_user_memory(&self) -> bool {
        self.accessing_user_memory.load(Ordering::Acquire)
    }

    /// Marks whether the thread is currently performing a user-memory access.
    pub fn set_accessing_user_memory(&self, accessing: bool) {
        self.accessing_user_memory
            .store(accessing, Ordering::Release);
    }

    /// Installs a per-thread TEE session context if one is not present yet.
    /// Accepts a preallocated owner without allocation. A redundant owner is
    /// dropped after releasing the slot mutex; its destructor runs in caller context.
    #[cfg(feature = "tee")]
    pub fn set_tee_session_ctx(&self, ctx: KBox<dyn TeeSessionCtxTrait>) {
        let mut guard = self.tee_session_ctx.lock();
        if guard.is_none() {
            *guard = Some(ctx);
        }
    }

    /// Executes `f` with mutable access to the optional per-thread TEE session context.
    /// Holds the slot mutex across `f`: do not reenter it or use allocations
    /// whose reclaim path may require this mutex. Returning an owner transfers
    /// its destruction responsibility to the caller.
    #[cfg(feature = "tee")]
    pub fn with_tee_session_ctx_mut<R>(
        &self,
        f: impl FnOnce(&mut Option<KBox<dyn TeeSessionCtxTrait>>) -> R,
    ) -> R {
        f(&mut self.tee_session_ctx.lock())
    }

    /// Executes `f` with shared access to the optional per-thread TEE session context.
    /// Holds the slot mutex across `f`; the same context restrictions as
    /// [`Self::with_tee_session_ctx_mut`] apply.
    #[cfg(feature = "tee")]
    pub fn with_tee_session_ctx<R>(
        &self,
        f: impl FnOnce(&Option<KBox<dyn TeeSessionCtxTrait>>) -> R,
    ) -> R {
        f(&self.tee_session_ctx.lock())
    }

    /// Returns the stable process identity that owns this thread.
    pub fn process(&self) -> &Arc<Process> {
        &self.process
    }

    /// Returns this task's objective credentials.
    pub fn real_cred(&self) -> Arc<Cred> {
        self.real_cred.read().clone()
    }

    pub(super) fn subjective_cred(&self) -> Arc<Cred> {
        self.cred.read().clone()
    }

    pub(super) fn commit_cred(&self, new: Cred) {
        let mut real_cred = self.real_cred.write();
        let mut cred = self.cred.write();
        assert!(
            Arc::ptr_eq(&real_cred, &cred),
            "committing credentials while subjective credentials are overridden"
        );
        let new = Arc::new(new);
        *real_cred = new.clone();
        *cred = new;
    }

    /// Replaces both credential pointers on a non-current task for tests.
    ///
    /// Production credential changes run on the current task. Tests need to
    /// give a published fixture distinct objective credentials before checking
    /// lookups that intentionally read another task's `real_cred`.
    #[cfg(unittest)]
    pub(crate) fn replace_cred_for_test(&self, cred: Cred) {
        self.commit_cred(cred);
    }

    /// Returns the thread-level signal manager.
    pub fn signal_manager(&self) -> &Arc<ThreadSignalManager> {
        &self.signal
    }

    /// Returns this thread's task-number identity (`struct pid`), which names
    /// the thread in every PID namespace that holds a projection for it.
    pub fn task_number(&self) -> Arc<kidentity::PidHandle> {
        self.task_number.lock().clone()
    }

    /// Adopts `number` as this thread's current number binding.
    ///
    /// Used by the `exec` leader takeover after the global number directory has
    /// been moved; only the thread-local binding is updated here. The numeric
    /// snapshot is written in the same critical section so `tid()` never sees
    /// the two apart.
    pub(crate) fn adopt_task_number(&self, number: Arc<kidentity::PidHandle>) {
        let mut current = self.task_number.lock();
        *current = number;
        self.tid.store(current.root_nr(), Ordering::Relaxed);
    }

    /// Returns the stable identity token for this thread.
    pub fn identity_token(&self) -> kidentity::TaskIdentityToken {
        self.identity_token
    }

    pub(crate) fn process_runtime(&self) -> Arc<ProcessRuntime> {
        self.runtime.clone()
    }

    pub(crate) fn set_process_mm_resident_cpu(&self, cpu_id: kcpu_id_map::LogicalCpuId) {
        let exit_address_space = self.exit_address_space.lock();
        if let Some(snapshot) = exit_address_space.as_ref() {
            snapshot.set_mm_resident_cpu(cpu_id);
        } else {
            self.runtime.set_mm_resident_cpu(cpu_id);
        }
    }

    #[cfg(target_arch = "aarch64")]
    pub(crate) fn process_page_table_hw_root(&self) -> karch::HwPageTableRoot {
        let exit_address_space = self.exit_address_space.lock();
        if let Some(snapshot) = exit_address_space.as_ref() {
            snapshot.page_table_hw_root()
        } else {
            self.runtime.page_table_hw_root()
        }
    }

    /// Pins the current scheduling context before this thread becomes unpublished.
    /// No allocation or MM mutex; lock order is thread exit lock, then binding.
    /// Never replace an installed owner, even on repeated exit.
    pub(crate) fn capture_exit_address_space(&self) {
        let mut snapshot = self.exit_address_space.lock();
        if snapshot.is_none() {
            *snapshot = Some(self.runtime.capture_exit_address_space());
        }
    }

    #[cfg(unittest)]
    pub(crate) fn has_exit_address_space(&self) -> bool {
        self.exit_address_space.lock().is_some()
    }

    /// Inspects the retained tuple without preparing an ASID on the test CPU.
    #[cfg(unittest)]
    pub(crate) fn exit_address_space_matches(
        &self,
        matches: impl FnOnce(&ExitAddressSpace) -> bool,
    ) -> bool {
        self.exit_address_space.lock().as_ref().is_some_and(matches)
    }

    /// Returns the owning process ID under the current root/global PID semantics.
    pub fn pid(&self) -> Pid {
        self.process().pid()
    }

    /// Reads the committed raw user/system counters.
    ///
    /// Lock-free and settlement-free: a query may sample another thread from
    /// any context that may read it. Only the owning CPU's periodic tick writes
    /// these counters, so a remote read never advances them.
    pub(crate) fn raw_cpu_time(&self) -> (u64, u64) {
        self.time.raw()
    }

    /// Charges one periodic accounting tick to the interrupted context.
    ///
    /// Owning-CPU only, from the periodic tick with IRQs masked. `user_tick`
    /// classifies the interrupted frame; `tick_ns` is `TICK_NSEC`.
    pub(crate) fn account_tick(&self, user_tick: bool, tick_ns: u64) {
        self.time.account_tick(user_tick, tick_ns);
    }

    /// Returns this thread's adjusted user/system time.
    ///
    /// `rtime_ns` is the thread's scheduler-runtime sample. The caller refreshes
    /// the current task before reading it; a remote thread contributes its
    /// committed runtime. The raw tick split is scaled against that runtime and
    /// kept monotonic by `prev_cputime`.
    pub fn cpu_time_adjusted(&self, rtime_ns: u64) -> (TimeSpan, TimeSpan) {
        let (utime, stime) = self.time.raw();
        let (utime, stime) = self.prev_cputime.adjust(utime, stime, rtime_ns);
        (TimeSpan::from_nanos(utime), TimeSpan::from_nanos(stime))
    }

    /// Requests a CPU-timer poll for this task at its next user return.
    ///
    /// Called from interrupt context by the periodic tick. Requests are
    /// coalesced: the `Release` store publishes this tick's raw counter
    /// additions, and repeated ticks before a return add no work. Gated by
    /// the process armed flag, so a task group with no CPU timers never sets
    /// the bit. See [`Thread::cpu_timer_work_pending`] for the ordering
    /// contract.
    pub(crate) fn request_cpu_timer_work(&self) {
        if self.process().cpu_timers_armed() {
            self.cpu_timer_work_pending.store(true, Ordering::Release);
        }
    }

    /// Returns whether a periodic tick requested CPU-timer work for this task.
    ///
    /// Lock-free probe for the user-return fast path; the `Acquire` load pairs
    /// with the tick's `Release` store. A `true` answer may be consumed only
    /// by [`Thread::take_cpu_timer_work`].
    pub fn has_pending_cpu_timer_work(&self) -> bool {
        self.cpu_timer_work_pending.load(Ordering::Acquire)
    }

    /// Consumes one coalesced CPU-timer work request.
    ///
    /// Returns `true` at most once per tick request; the `AcqRel` swap both
    /// observes the requesting tick's raw additions and orders a concurrent
    /// re-request after this consumption. See
    /// [`Thread::cpu_timer_work_pending`] for the full ordering contract.
    pub(crate) fn take_cpu_timer_work(&self) -> bool {
        self.cpu_timer_work_pending.swap(false, Ordering::AcqRel)
    }

    /// Temporarily swaps the blocked-signal mask while executing `f`.
    ///
    /// # Errors
    ///
    /// Forwards the closure error through the signal manager temporary-mask
    /// operation; mask restoration follows that provider contract.
    pub fn with_temp_blocked<R>(
        &self,
        blocked: Option<ksignal::SignalSet>,
        f: impl FnOnce() -> kerrno::KResult<R>,
    ) -> kerrno::KResult<R> {
        self.signal.with_temp_blocked(blocked, f)
    }
}

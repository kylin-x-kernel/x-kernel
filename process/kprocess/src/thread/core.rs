// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, sync::Arc};
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicUsize, Ordering};

use kcred::Cred;
use kerrno::KResult;
use khal::time::TimeValue;
use ksignal::{SignalStack, api::ThreadSignalManager};
use ksync::{Mutex, RwLock};
use ktask::KtaskRef;
#[cfg(feature = "tee")]
use tee_task_iface::TeeSessionCtxTrait;

use super::cpu_time::{CpuTimeState, CpuTimeStatistics};
use crate::{
    Pid, Process, ProcessForkConfig, ProcessRuntime, Tid, allocate_thread_task_number,
    fork_process_runtime,
};

/// The current user thread handle.
pub struct CurrentThread(pub(super) KtaskRef);

/// Fully prepared user-thread clone/fork result.
///
/// This bundles the preallocated thread identity together with the matching
/// thread object so higher layers can construct the target task without
/// re-implementing PID namespace or identity-allocation policy.
pub struct PreparedUserClone {
    thread: Box<Thread>,
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
    pub fn page_table_root(&self) -> karch::HwPageTableRoot {
        self.thread
            .process()
            .address_space()
            .expect("prepared user clone must have a live process address space")
            .lock()
            .page_table_hw_root()
    }

    /// Splits the prepared clone into the thread object and its task identity.
    pub fn into_parts(self) -> (Box<Thread>, Arc<kidentity::PidHandle>) {
        (self.thread, self.task_number)
    }
}

/// The inner data of a thread.
pub struct Thread {
    task_number: Arc<kidentity::PidHandle>,
    process: Arc<Process>,
    runtime: Arc<ProcessRuntime>,
    real_cred: RwLock<Arc<Cred>>,
    cred: RwLock<Arc<Cred>>,
    clear_child_tid: AtomicUsize,
    robust_list_head: AtomicUsize,
    signal: Arc<ThreadSignalManager>,
    time: Mutex<CpuTimeStatistics>,
    oom_score_adj: AtomicI32,
    nice: AtomicI32,
    scheduler_policy: AtomicU32,
    scheduler_priority: AtomicI32,
    exit: AtomicBool,
    accessing_user_memory: AtomicBool,
    #[cfg(feature = "tee")]
    tee_session_ctx: Mutex<Option<Box<dyn TeeSessionCtxTrait>>>,
}

impl Thread {
    /// Creates a new [`Thread`].
    pub(crate) fn new(
        process: Arc<Process>,
        runtime: Arc<ProcessRuntime>,
        task_number: Arc<kidentity::PidHandle>,
        cred: Arc<Cred>,
    ) -> Box<Self> {
        let tid = task_number.root_nr();
        Box::new(Thread {
            task_number,
            signal: ThreadSignalManager::new(tid, runtime.signal_manager().clone()),
            process,
            runtime,
            real_cred: RwLock::new(cred.clone()),
            cred: RwLock::new(cred),
            clear_child_tid: AtomicUsize::new(0),
            robust_list_head: AtomicUsize::new(0),
            time: Mutex::new(CpuTimeStatistics::new()),
            oom_score_adj: AtomicI32::new(200),
            nice: AtomicI32::new(0),
            scheduler_policy: AtomicU32::new(u32::MAX),
            scheduler_priority: AtomicI32::new(0),
            exit: AtomicBool::new(false),
            accessing_user_memory: AtomicBool::new(false),
            #[cfg(feature = "tee")]
            tee_session_ctx: Mutex::new(None),
        })
    }

    /// Forks a child process from this thread's current process and returns the
    /// child thread object for installation into a new task.
    pub fn fork_process_child(&self, config: ProcessForkConfig) -> KResult<Box<Self>> {
        self.prepare_process_fork(config)
            .map(|prepared| prepared.into_parts().0)
    }

    /// Prepares a child process clone together with its leader task identity.
    pub fn prepare_process_fork(&self, config: ProcessForkConfig) -> KResult<PreparedUserClone> {
        let (process_runtime, task_number) = fork_process_runtime(&self.process_runtime(), config)?;
        let thread = Self::new(
            process_runtime.process().clone(),
            process_runtime,
            task_number.clone(),
            self.subjective_cred(),
        );
        Ok(PreparedUserClone {
            thread,
            task_number,
        })
    }

    /// Creates a sibling thread within the same process runtime.
    pub fn clone_thread_in_process(&self) -> KResult<Box<Self>> {
        self.prepare_thread_clone()
            .map(|prepared| prepared.into_parts().0)
    }

    /// Prepares a sibling thread clone together with its thread identity.
    pub fn prepare_thread_clone(&self) -> KResult<PreparedUserClone> {
        let task_number = allocate_thread_task_number()?;
        let thread = Self::new(
            self.process.clone(),
            self.runtime.clone(),
            task_number.clone(),
            self.subjective_cred(),
        );
        Ok(PreparedUserClone {
            thread,
            task_number,
        })
    }

    /// Returns the user-visible thread identifier.
    pub fn tid(&self) -> Tid {
        self.task_number.root_nr()
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
    pub fn reset_after_exec(&self) {
        self.set_clear_child_tid(0);
        self.set_robust_list_head(0);
        self.signal.set_stack(SignalStack::default());
        #[cfg(feature = "tee")]
        {
            *self.tee_session_ctx.lock() = None;
        }
    }

    /// Returns the robust-futex list head pointer registered by this thread.
    pub fn robust_list_head(&self) -> usize {
        self.robust_list_head.load(Ordering::SeqCst)
    }

    /// Sets the robust-futex list head pointer for this thread.
    pub fn set_robust_list_head(&self, robust_list_head: usize) {
        self.robust_list_head
            .store(robust_list_head, Ordering::SeqCst);
    }

    /// Returns the thread's current OOM score adjustment.
    pub fn oom_score_adj(&self) -> i32 {
        self.oom_score_adj.load(Ordering::SeqCst)
    }

    /// Sets the thread's OOM score adjustment.
    pub fn set_oom_score_adj(&self, value: i32) {
        self.oom_score_adj.store(value, Ordering::SeqCst);
    }

    /// Returns the thread's scheduler nice value.
    pub fn nice(&self) -> i32 {
        self.nice.load(Ordering::Acquire)
    }

    /// Sets the thread's scheduler nice value.
    pub fn set_nice(&self, nice: i32) {
        self.nice.store(nice, Ordering::Release);
    }

    /// Returns the explicit scheduler policy if one has been configured.
    pub fn scheduler_policy(&self) -> Option<u32> {
        match self.scheduler_policy.load(Ordering::Acquire) {
            u32::MAX => None,
            policy => Some(policy),
        }
    }

    /// Returns the configured scheduler priority.
    pub fn scheduler_priority(&self) -> i32 {
        self.scheduler_priority.load(Ordering::Acquire)
    }

    /// Sets the scheduler policy and priority for this thread.
    pub fn set_scheduler(&self, policy: u32, priority: i32) {
        self.scheduler_priority.store(priority, Ordering::Release);
        self.scheduler_policy.store(policy, Ordering::Release);
    }

    /// Returns whether the thread is in its exit path.
    pub fn is_exiting(&self) -> bool {
        self.exit.load(Ordering::Acquire)
    }

    /// Marks the thread as exiting.
    pub fn set_exit(&self) {
        self.exit.store(true, Ordering::Release);
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

    #[cfg(feature = "tee")]
    /// Installs a per-thread TEE session context if one is not present yet.
    pub fn set_tee_session_ctx(&self, ctx: Box<dyn TeeSessionCtxTrait>) {
        let mut guard = self.tee_session_ctx.lock();
        if guard.is_none() {
            *guard = Some(ctx);
        }
    }

    #[cfg(feature = "tee")]
    /// Executes `f` with mutable access to the optional per-thread TEE session context.
    pub fn with_tee_session_ctx_mut<R>(
        &self,
        f: impl FnOnce(&mut Option<Box<dyn TeeSessionCtxTrait>>) -> R,
    ) -> R {
        f(&mut self.tee_session_ctx.lock())
    }

    #[cfg(feature = "tee")]
    /// Executes `f` with shared access to the optional per-thread TEE session context.
    pub fn with_tee_session_ctx<R>(
        &self,
        f: impl FnOnce(&Option<Box<dyn TeeSessionCtxTrait>>) -> R,
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

    /// Returns the thread-level signal manager.
    pub fn signal_manager(&self) -> &Arc<ThreadSignalManager> {
        &self.signal
    }

    pub(crate) fn task_number(&self) -> &Arc<kidentity::PidHandle> {
        &self.task_number
    }

    pub(crate) fn process_runtime(&self) -> Arc<ProcessRuntime> {
        self.runtime.clone()
    }

    pub(crate) fn set_process_mm_resident_cpu(&self, cpu_id: kcpu_id_map::LogicalCpuId) {
        self.process_runtime().set_mm_resident_cpu(cpu_id);
    }

    #[cfg(target_arch = "aarch64")]
    pub(crate) fn process_page_table_hw_root(&self) -> karch::HwPageTableRoot {
        self.process_runtime().page_table_hw_root()
    }

    /// Returns the owning process ID under the current root/global PID semantics.
    pub fn pid(&self) -> Pid {
        self.process().pid()
    }

    /// Returns sampled user and kernel CPU time in nanoseconds.
    pub fn sample_cpu_time_ns(&self) -> (usize, usize) {
        self.time.lock().sample_nanos()
    }

    /// Returns sampled user and kernel CPU time.
    pub fn sample_cpu_time(&self) -> (TimeValue, TimeValue) {
        self.time.lock().sample()
    }

    /// Returns the sum of user and kernel CPU time consumed by this thread.
    pub fn cpu_time(&self) -> TimeValue {
        let (utime, stime) = self.sample_cpu_time();
        utime + stime
    }

    /// Updates the accounting state used for subsequent CPU-time samples.
    pub fn set_cpu_state(&self, state: CpuTimeState) {
        self.time.lock().set_state(state);
    }

    /// Temporarily swaps the blocked-signal mask while executing `f`.
    pub fn with_temp_blocked<R>(
        &self,
        blocked: Option<ksignal::SignalSet>,
        f: impl FnOnce() -> kerrno::KResult<R>,
    ) -> kerrno::KResult<R> {
        self.signal.with_temp_blocked(blocked, f)
    }
}

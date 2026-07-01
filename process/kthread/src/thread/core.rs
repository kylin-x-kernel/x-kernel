// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, sync::Arc};
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicUsize, Ordering};

use khal::time::TimeValue;
use kprocess::Pid;
use ksignal::api::ThreadSignalManager;
use ksync::Mutex;
use ktask::KtaskRef;
#[cfg(feature = "tee")]
use tee_task_iface::TeeSessionCtxTrait;

use crate::{CpuTimeState, CpuTimeStatistics, ProcessState};

/// The current user thread handle.
pub struct CurrentThread(pub(super) KtaskRef);

/// The inner data of a thread.
pub struct Thread {
    /// The process state shared by all threads in the process.
    pub proc_state: Arc<ProcessState>,

    /// The clear thread tid field.
    clear_child_tid: AtomicUsize,

    /// The head of the robust list.
    robust_list_head: AtomicUsize,

    /// The thread-level signal manager.
    pub signal: Arc<ThreadSignalManager>,

    /// Per-thread CPU time statistics.
    pub time: Mutex<CpuTimeStatistics>,

    /// The OOM score adjustment value.
    oom_score_adj: AtomicI32,

    /// The Linux nice value for ordinary scheduler priority.
    nice: AtomicI32,

    /// Linux scheduler policy stored for syscall ABI compatibility.
    scheduler_policy: AtomicU32,

    /// Linux scheduler priority stored for syscall ABI compatibility.
    scheduler_priority: AtomicI32,

    /// Indicates whether the thread is ready to exit.
    exit: AtomicBool,

    /// Indicates whether the thread is currently accessing user memory.
    accessing_user_memory: AtomicBool,

    /// The TEE session context.
    #[cfg(feature = "tee")]
    pub tee_session_ctx: Mutex<Option<Box<dyn TeeSessionCtxTrait>>>,
}

impl Thread {
    /// Creates a new [`Thread`].
    pub fn new(tid: u32, proc_state: Arc<ProcessState>) -> Box<Self> {
        Box::new(Thread {
            signal: ThreadSignalManager::new(tid, proc_state.signal.clone()),
            proc_state,
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

    /// Returns the clear-child-tid field.
    pub fn clear_child_tid(&self) -> usize {
        self.clear_child_tid.load(Ordering::Relaxed)
    }

    /// Sets the clear-child-tid field.
    pub fn set_clear_child_tid(&self, clear_child_tid: usize) {
        self.clear_child_tid
            .store(clear_child_tid, Ordering::Relaxed);
    }

    /// Returns the robust-list head.
    pub fn robust_list_head(&self) -> usize {
        self.robust_list_head.load(Ordering::SeqCst)
    }

    /// Sets the robust-list head.
    pub fn set_robust_list_head(&self, robust_list_head: usize) {
        self.robust_list_head
            .store(robust_list_head, Ordering::SeqCst);
    }

    /// Returns the OOM score adjustment value.
    pub fn oom_score_adj(&self) -> i32 {
        self.oom_score_adj.load(Ordering::SeqCst)
    }

    /// Sets the OOM score adjustment value.
    pub fn set_oom_score_adj(&self, value: i32) {
        self.oom_score_adj.store(value, Ordering::SeqCst);
    }

    /// Returns the Linux nice value for ordinary scheduler priority.
    pub fn nice(&self) -> i32 {
        self.nice.load(Ordering::Acquire)
    }

    /// Sets the Linux nice value for ordinary scheduler priority.
    pub fn set_nice(&self, nice: i32) {
        self.nice.store(nice, Ordering::Release);
    }

    /// Returns the Linux scheduler policy set through scheduler syscalls.
    pub fn scheduler_policy(&self) -> Option<u32> {
        match self.scheduler_policy.load(Ordering::Acquire) {
            u32::MAX => None,
            policy => Some(policy),
        }
    }

    /// Returns the Linux scheduler priority set through scheduler syscalls.
    pub fn scheduler_priority(&self) -> i32 {
        self.scheduler_priority.load(Ordering::Acquire)
    }

    /// Stores Linux scheduler state for syscall ABI compatibility.
    pub fn set_scheduler(&self, policy: u32, priority: i32) {
        self.scheduler_priority.store(priority, Ordering::Release);
        self.scheduler_policy.store(policy, Ordering::Release);
    }

    /// Returns whether the thread is exiting.
    pub fn is_exiting(&self) -> bool {
        self.exit.load(Ordering::Acquire)
    }

    /// Marks the thread as exiting.
    pub fn set_exit(&self) {
        self.exit.store(true, Ordering::Release);
    }

    /// Returns whether the thread is currently accessing user memory.
    pub fn is_accessing_user_memory(&self) -> bool {
        self.accessing_user_memory.load(Ordering::Acquire)
    }

    /// Sets the accessing-user-memory flag.
    pub fn set_accessing_user_memory(&self, accessing: bool) {
        self.accessing_user_memory
            .store(accessing, Ordering::Release);
    }

    /// Sets the TEE session context.
    #[cfg(feature = "tee")]
    pub fn set_tee_session_ctx(&self, ctx: Box<dyn TeeSessionCtxTrait>) {
        let mut guard = self.tee_session_ctx.lock();
        if guard.is_none() {
            *guard = Some(ctx);
        }
    }

    /// Returns the shared process state for this thread.
    pub fn process_state(&self) -> &Arc<ProcessState> {
        &self.proc_state
    }

    /// Returns the process ID of this thread's process.
    pub fn pid(&self) -> Pid {
        self.process_state().proc.pid()
    }

    /// Returns the sampled user and system CPU time in nanoseconds.
    pub fn sample_cpu_time_ns(&self) -> (usize, usize) {
        self.time.lock().sample_nanos()
    }

    /// Returns the sampled user and system CPU time.
    pub fn sample_cpu_time(&self) -> (TimeValue, TimeValue) {
        self.time.lock().sample()
    }

    /// Returns the total sampled CPU time (user + system).
    pub fn cpu_time(&self) -> TimeValue {
        let (utime, stime) = self.sample_cpu_time();
        utime + stime
    }

    /// Updates the current CPU-accounting state.
    pub fn set_cpu_state(&self, state: CpuTimeState) {
        self.time.lock().set_state(state);
    }

    /// Temporarily replaces the blocked signal set for the duration of `f`,
    /// then restores the original set.  See [`ThreadSignalManager::with_temp_blocked`].
    pub fn with_temp_blocked<R>(
        &self,
        blocked: Option<ksignal::SignalSet>,
        f: impl FnOnce() -> kerrno::KResult<R>,
    ) -> kerrno::KResult<R> {
        self.signal.with_temp_blocked(blocked, f)
    }
}

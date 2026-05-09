// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, sync::Arc};
use core::{
    cell::RefCell,
    ops::Deref,
    sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering},
};

use kprocess::Pid;
use ksignal::api::ThreadSignalManager;
#[cfg(feature = "tee")]
use ksync::Mutex;
use ktask::KtaskRef;
use ktimer::TimeManager;
#[cfg(feature = "tee")]
use tee_task_iface::TeeSessionCtxTrait;

use crate::ProcessState;

/// A wrapper type that assumes the inner type is `Sync`.
#[repr(transparent)]
pub struct AssumeSync<T>(pub T);

// SAFETY: `AssumeSync` wraps `RefCell<TimeManager>`, which is only ever accessed from the
// owning thread (single-threaded access). No concurrent mutation occurs, so marking it
// `Sync` is sound as long as all access remains single-threaded.
unsafe impl<T> Sync for AssumeSync<T> {}

impl<T> Deref for AssumeSync<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

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

    /// The time manager.
    pub time: AssumeSync<RefCell<TimeManager>>,

    /// The OOM score adjustment value.
    oom_score_adj: AtomicI32,

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
            time: AssumeSync(RefCell::new(TimeManager::new())),
            oom_score_adj: AtomicI32::new(200),
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
}

#[cfg(unittest)]
mod tests_thread {
    use unittest::def_test;

    use super::AssumeSync;

    #[def_test]
    fn test_assume_sync_deref() {
        let value = AssumeSync(42_u32);
        assert_eq!(*value, 42);
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Allocation-free task waits for synchronous completion and predicate queues.

use alloc::sync::Arc;
use core::marker::PhantomData;

use kcompletion::{Completion, SyncWake, WaitQueue};
use kerrno::{KError, KResult};
use kspin::NoPreemptIrqSave;

use crate::{KtaskRef, TaskState, current, select_wake_run_queue};

struct CompletionWake {
    task: KtaskRef,
    /// Shared latch: every waker/notifier clone observes the same wake hint.
    woke: Arc<crate::park::ParkLatch>,
}

// SAFETY: notify borrows only this live, lock-protected latch and strong task
// reference. It neither allocates nor retains the borrow, reenters the wait
// source, or unwinds. The latch is released before IRQ-safe scheduler wakeup.
unsafe impl SyncWake for CompletionWake {
    fn notify(&self) {
        self.woke.record();
        select_wake_run_queue::<NoPreemptIrqSave>(&self.task).unblock_task(self.task.clone(), true);
    }
}

impl CompletionWait {
    /// Clones the task's IRQ-safe notifier as a standard `Waker`.
    ///
    /// A wake source may store this waker in its own registry: the clone keeps
    /// its own strong task reference, rides the same wake latch as
    /// [`Self::notifier`], and revives nothing once this handle is dropped.
    pub fn notifier_waker(&self) -> core::task::Waker {
        core::task::Waker::from(alloc::sync::Arc::new(SharedWake(self.wake.clone())))
    }
}

impl Clone for CompletionWake {
    fn clone(&self) -> Self {
        Self {
            task: self.task.clone(),
            woke: self.woke.clone(),
        }
    }
}

/// [`CompletionWake`] behind an `Arc`, so it can back a cloned waker.
struct SharedWake(CompletionWake);

impl alloc::task::Wake for SharedWake {
    fn wake(self: alloc::sync::Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &alloc::sync::Arc<Self>) {
        self.0.notify();
    }
}

/// Task-bound resources for allocation-free synchronous waits.
///
/// Only the preparing task may use this handle; it is neither Send nor Sync.
/// A strong current-task reference survives every context switch. Wait nodes
/// borrow these resources only for the duration of each wait.
pub struct CompletionWait {
    wake: CompletionWake,
    _task_bound: PhantomData<*mut ()>,
}

impl CompletionWait {
    /// Borrows the IRQ-safe notifier for a scoped, pinned wait registration.
    ///
    /// The source must unlink its registration before this borrow ends. The
    /// notifier cannot escape or own the task-bound parking handle.
    pub fn notifier(&self) -> &impl SyncWake {
        &self.wake
    }

    /// Returns whether this task has an unblockable termination request.
    /// Kernel tasks without a user runtime return false.
    pub fn is_killed(&self) -> bool {
        self.wake
            .task
            .user_runtime()
            .is_some_and(|runtime| runtime.fatal_signal_pending())
    }

    /// Parks an enrolled lock waiter, recording its address for diagnostics.
    /// Returns whether the task actually blocked. Signals do not end the wait.
    /// `on_block` runs once immediately before blocking, under IRQ/preemption
    /// exclusion. It must only update nonblocking diagnostics, without acquiring
    /// source or scheduler locks, allocating, panicking or changing context.
    /// Requires the preparing task in sleepable context with source locks released.
    ///
    /// # Panics
    /// Panics if called outside the preparing task or sleepable context.
    pub fn park_lock(&self, address: usize, on_block: impl FnOnce()) -> bool {
        self.park_lock_unless(address, TaskState::Blocked, || false, on_block)
    }

    /// Parks an enrolled lock waiter unless termination is pending.
    /// The caller must recheck both its grant and `is_killed` after return.
    /// A signal producer must publish pending state before `signal_task`.
    /// Context requirements and the return value match `park_lock`.
    ///
    /// # Panics
    /// Panics if called outside the preparing task or sleepable context.
    pub fn park_lock_killable(&self, address: usize, on_block: impl FnOnce()) -> bool {
        self.park_lock_unless(
            address,
            TaskState::BlockedKillable,
            || self.is_killed(),
            on_block,
        )
    }

    fn park_lock_unless(
        &self,
        address: usize,
        state: TaskState,
        stop: impl FnOnce() -> bool,
        on_block: impl FnOnce(),
    ) -> bool {
        self.validate_context();
        #[cfg(feature = "snapshot")]
        self.wake
            .task
            .set_waiting_lock(address, khal::time::now_ticks());
        #[cfg(not(feature = "snapshot"))]
        let _ = address;
        let _diagnostic = WaitingLock(&self.wake.task);
        crate::park::park_task_unless(&self.wake.woke, state, stop, on_block)
    }

    /// Captures the current task without allocating.
    ///
    /// # Errors
    /// Returns [`KError::InvalidInput`] unless a non-idle current task exists
    /// in sleepable context: IRQs enabled, outside interrupt/BH-disabled and
    /// RCU read sections, and with preemption enabled where tracked.
    pub fn prepare() -> KResult<Self> {
        let curr = crate::current_may_uninit().ok_or(KError::InvalidInput)?;
        if !crate::is_sleepable_context() {
            return Err(KError::InvalidInput);
        }
        Ok(Self {
            wake: CompletionWake {
                task: curr.clone(),
                woke: Arc::new(crate::park::ParkLatch::new()),
            },
            _task_bound: PhantomData,
        })
    }

    /// Waits for a completion token or persistent broadcast completion.
    ///
    /// Registration uses a scoped stack node and cannot fail. Spurious wakes
    /// recheck completion; signals do not interrupt the wait.
    ///
    /// # Panics
    /// Panics if called outside the preparing task or sleepable context.
    pub fn wait(&self, completion: &Completion) {
        self.validate_context();
        // SAFETY: the only park closure uses the scheduler handshake and keeps
        // this handle borrowed. Its strong task ref retains TaskInner::kstack
        // across switches; even a non-unwinding exit cannot free that stack
        // without dropping this ref. Normal return/unwind unlinks the waiter.
        unsafe { completion.wait_with(&self.wake, || crate::park::park_task(&self.wake.woke)) };
    }

    /// Waits until `ready` returns true, with allocation-free registration.
    ///
    /// The predicate runs after registration and outside the queue lock, and
    /// is rechecked after spurious wakes. Producers must publish predicate
    /// changes before notifying the queue. The predicate must not sleep or
    /// recursively wait, and must leave the task in sleepable context.
    /// Exiting the task from the predicate without unwinding leaks this handle's
    /// task reference and preserves its kernel stack.
    ///
    /// # Panics
    /// Panics if called outside the preparing task or sleepable context.
    pub fn wait_until(&self, queue: &WaitQueue, ready: impl FnMut() -> bool) {
        self.validate_context();
        // SAFETY: this borrowed handle retains the current task and its owned
        // kernel stack for the whole scoped registration. If ready exits the
        // task without unwinding, this undropped strong ref prevents TaskStack
        // destruction, retaining the pinned node and wake storage. Normal
        // return/unwind unregisters before the handle can be dropped.
        unsafe {
            queue.wait_until(&self.wake, ready, || {
                crate::park::park_task(&self.wake.woke)
            })
        };
    }

    fn validate_context(&self) {
        assert!(
            current().ptr_eq(&self.wake.task),
            "completion wait belongs to another task"
        );
        assert!(
            crate::is_sleepable_context(),
            "completion wait requires sleepable context"
        );
    }
}

struct WaitingLock<'a>(&'a KtaskRef);

impl Drop for WaitingLock<'_> {
    fn drop(&mut self) {
        #[cfg(feature = "snapshot")]
        self.0.clear_waiting_lock();
        #[cfg(not(feature = "snapshot"))]
        let _ = self.0;
    }
}

/// Validates task context and waits without allocating or registration errors.
///
/// # Errors
/// Returns [`KError::InvalidInput`] in a non-sleepable context, even when the
/// completion is already signaled.
pub fn wait_for_completion(completion: &Completion) -> KResult<()> {
    CompletionWait::prepare()?.wait(completion);
    Ok(())
}

#[cfg(unittest)]
unittest::path_mod_test!(tests, "../ut-tests/completion/tests.rs");

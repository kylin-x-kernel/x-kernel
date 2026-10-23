// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Allocation-free task waits for synchronous completion and predicate queues.

use core::marker::PhantomData;

use kcompletion::{Completion, SyncWake, WaitQueue};
use kerrno::{KError, KResult};
use kspin::{NoPreemptIrqSave, SpinNoIrq};

use crate::{KtaskRef, current, select_wake_run_queue};

struct CompletionWake {
    task: KtaskRef,
    woke: SpinNoIrq<bool>,
}

// SAFETY: notify borrows only this live, lock-protected latch and strong task
// reference. It neither allocates nor retains the borrow, reenters the wait
// source, or unwinds. The latch is released before IRQ-safe scheduler wakeup.
unsafe impl SyncWake for CompletionWake {
    fn notify(&self) {
        *self.woke.lock() = true;
        select_wake_run_queue::<NoPreemptIrqSave>(&self.task).unblock_task(self.task.clone(), true);
    }
}

/// Task-bound resources for allocation-free, uninterruptible synchronous waits.
///
/// Only the preparing task may use this handle; it is neither Send nor Sync.
/// A strong current-task reference survives every context switch. Wait nodes
/// borrow these resources only for the duration of each wait.
pub struct CompletionWait {
    wake: CompletionWake,
    _task_bound: PhantomData<*mut ()>,
}

impl CompletionWait {
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
                woke: SpinNoIrq::new(false),
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

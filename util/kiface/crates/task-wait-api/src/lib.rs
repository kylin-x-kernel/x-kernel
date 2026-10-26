// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Task-bound completion waiting, independent of IRQ or RCU policy.
//!
//! This contract layer uses kcompletion; the lower-level kiface-task-api remains
//! usable by kspin, which kcompletion itself needs. No scheduler or wait state
//! is owned here. The task runtime supplies the sole provider.

#![no_std]

use core::task::Waker;

use kcompletion::Completion;
use kerrno::KResult;

/// Prepared completion waiting bound to the preparing task.
///
/// The provider retains this object and a strong task reference throughout the
/// synchronous body. The borrowed handle cannot be transferred to another task.
pub trait PreparedCompletionWait {
    /// Acquires a completion token or observes persistent broadcast completion.
    ///
    /// Requires the preparing task in sleepable context, with no lock needed
    /// by the producer. Registration and parking do not allocate or fail.
    /// Signals and spurious wakes do not end the wait early. Acquiring a wake
    /// does not establish any subsystem predicate: consumers must recheck it.
    fn wait_for_completion(&self, completion: &Completion);
}

/// Allocation-free preparation and scoped borrowing of task wait resources.
#[kiface::interface]
pub trait TaskCompletionWaitIf {
    /// Validates context, then lends a prepared wait to the body exactly once.
    ///
    /// Before invocation, reject absent/idle current, masked IRQs, interrupt/BH
    /// context, preemption exclusion and active RCU read scopes. Retain the wait
    /// on this stack through the synchronous body. No allocation is permitted.
    /// After invoking the body return only Ok; never report failure after the
    /// consumer may have published work. The body must remain on this task and
    /// only wait in sleepable context. It may not retain the borrowed handle.
    ///
    /// # Errors
    /// Returns InvalidInput for a non-sleepable context without calling body,
    /// even if a completion the body intended to wait for is already signaled.
    ///
    /// ```compile_fail
    /// let mut escaped = None;
    /// let _ = kiface_task_wait_api::TaskCompletionWaitIf::with_prepared_wait(&mut |wait| {
    ///     escaped = Some(wait);
    /// });
    /// ```
    fn with_prepared_wait(body: &mut dyn FnMut(&mut dyn PreparedCompletionWait)) -> KResult<()>;
}

/// Validates current context and waits for completion without allocating.
///
/// # Errors
/// Returns InvalidInput if current context is not sleepable, including when
/// completion was already signaled. Completion is a wake source, not proof of
/// an IRQ, RCU or other consumer-owned lifecycle predicate.
pub fn wait_for_completion(completion: &Completion) -> KResult<()> {
    TaskCompletionWaitIf::with_prepared_wait(&mut |wait| wait.wait_for_completion(completion))
}

/// Prepared task-bound parking; wakeups are hints, not completion.
///
/// The host retains this object on the preparing task's stack. It must remember
/// a wake delivered before park and permit spurious returns. Neither method
/// registers a wait or allocates; all fallible preparation precedes the borrow.
/// The borrow is not Send/Sync and must not escape the synchronous adapter body.
pub trait PreparedParking {
    /// Clones the prepared waker without allocating. It may outlive this borrow.
    fn waker(&self) -> Waker;

    /// Parks this same task without returning an interruption error.
    ///
    /// Requires sleepable context and no reader/service lock. Must not consume
    /// pending signals; the caller rechecks its completion predicate on return.
    fn park(&mut self);
}

/// Preparation and task-bound parking using retained wake hints.
#[kiface::interface]
pub trait TaskParkingIf {
    /// Prepares service parking, then lends it to the body exactly once.
    ///
    /// Retains early wake hints and permits spurious returns. The body owns
    /// its readiness predicate. No allocation or interruption error occurs
    /// during park; preparation retains the host's fatal allocation policy.
    /// On failure do not invoke the body; after invoking it return only Ok.
    ///
    /// # Errors
    /// Rejects contexts where the current task cannot sleep before the body.
    fn with_prepared_parking(body_fn: &mut dyn FnMut(&mut dyn PreparedParking)) -> KResult<()>;
}

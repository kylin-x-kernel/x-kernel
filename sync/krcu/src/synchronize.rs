// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Task-bound synchronous waits; callback completion never destroys retired data.

use alloc::sync::Arc;
use core::{
    marker::PhantomData,
    ptr::NonNull,
    sync::atomic::{Ordering, fence},
};

use kcompletion::Completion;
use kerrno::{KError, KResult};
use kiface_task_wait_api::PreparedCompletionWait;
use krcu_core::CallbackNode;

use super::{SERVICE, callback::PreparedCallback};
use crate::host::with_prepared_wait;

#[repr(C)]
struct GracePeriodCompletion {
    node: CallbackNode,
    completion: Completion,
}

// The callback recovers the Arc allocation directly from its node pointer.
const _: () = assert!(core::mem::offset_of!(GracePeriodCompletion, node) == 0);

/// Single-use grace-period wait bound to its preparing task and provider scope.
///
/// Created only by [`with_prepared_grace_period`]. Preparation does not start a
/// grace period. Dropping this capability cancels its private marker without
/// submitting work, waiting, or reclaiming any consumer-owned data.
///
/// The capability cannot move to another task:
///
/// ```compile_fail
/// fn require_send<T: Send>() {}
/// require_send::<krcu::PreparedGracePeriodWait<'static>>();
/// ```
#[must_use = "submit after withdrawal, or drop to cancel the unused wait marker"]
pub struct PreparedGracePeriodWait<'wait> {
    callback: PreparedCallback,
    completion: Arc<GracePeriodCompletion>,
    wait: &'wait mut dyn PreparedCompletionWait,
    task_local: PhantomData<*mut ()>,
}

/// Prepares task waiting and callback storage before invoking `body_fn` once.
///
/// Preparation requires sleepable task context outside read scopes and atomic
/// exclusion. The body stays on that task; it may temporarily acquire writer
/// locks to withdraw publication, but must restore watched, sleepable context
/// before submitting the wait. This capability owns no retired data and does
/// not serialize writers. Consumers retain detached storage until successful
/// submission and completion, or leak it on abandonment.
///
/// Callback storage follows the kernel's fatal-OOM allocation policy. No marker
/// is submitted until [`PreparedGracePeriodWait::submit_and_wait`] is called.
///
/// # Errors
/// Returns `InvalidInput` for invalid task context or `BadState` before service
/// readiness, without invoking the body. No recoverable preparation error is
/// returned after the body has run.
///
/// The capability cannot escape the provider's stack scope:
///
/// ```compile_fail
/// let escaped = krcu::with_prepared_grace_period(|prepared| prepared);
/// drop(escaped);
/// ```
pub fn with_prepared_grace_period<R>(
    body_fn: impl FnOnce(PreparedGracePeriodWait<'_>) -> R,
) -> KResult<R> {
    with_prepared_wait(|wait| PreparedGracePeriodWait::prepare(wait).map(body_fn))?
}

impl<'wait> PreparedGracePeriodWait<'wait> {
    fn prepare(wait: &'wait mut dyn PreparedCompletionWait) -> KResult<Self> {
        SERVICE
            .get()
            .filter(|service| service.waker.get().is_some())
            .ok_or(KError::BadState)?;
        let completion = Arc::new(GracePeriodCompletion {
            node: CallbackNode::new(complete_grace_period),
            completion: Completion::new(),
        });
        let record = Arc::into_raw(completion.clone());
        // SAFETY: raw field projection preserves the original Arc provenance.
        let node = unsafe { &raw const (*record).node }.cast_mut();
        // SAFETY: Arc::into_raw is non-null and node is its initialized field.
        let node = unsafe { NonNull::new_unchecked(node) };
        // SAFETY: the independent raw Arc keeps this unqueued marker fixed.
        // Cancellation releases only that reference, never retired user data;
        // callback and cancellation both recover the same zero-offset Arc.
        // Completion resources permit cross-CPU ownership and nonblocking drop.
        let callback = unsafe { PreparedCallback::new(node, cancel_grace_period) };
        Ok(Self {
            callback,
            completion,
            wait,
            task_local: PhantomData,
        })
    }

    /// Submits a fresh marker and waits noninterruptibly for its grace period.
    ///
    /// Returns with completion ordering established, including when completion
    /// precedes parking or this task migrates. This is not a callback barrier;
    /// neither the marker nor this capability destroys consumer payloads.
    ///
    /// # Safety
    /// Call on the preparing task in restored watched, sleepable context: no
    /// read scope, IRQ/preemption exclusion, or lock needed by readers, scheduler
    /// or RCU progress may remain held. Withdraw every publication relevant to
    /// reclamation before submission, and retain the old storage until return.
    /// All protected accesses must use this kernel's RCU participation protocol.
    /// Keep the permanent service and its participants available throughout.
    /// Abandonment or a nonreturning wait never authorizes reclamation.
    ///
    /// # Panics
    /// Panics if submission violates the prepared context/service admission
    /// invariant. Consumers must not free withdrawn storage on that failure.
    pub unsafe fn submit_and_wait(self) {
        // SAFETY: preparation verified the permanent boot service. Every
        // executing CPU belongs to its fixed participant set. This fresh node
        // has one queue owner; its raw Arc survives through the nonblocking
        // callback, independently of the waiter's Arc. No RCU/scheduler lock
        // is held by this sleepable synchronous path.
        let submitted = unsafe { self.callback.submit() };
        let is_accepted = match submitted {
            Ok(()) => true,
            Err((_, callback)) => {
                drop(callback);
                false
            }
        };
        // Startup publishes the waker only after claiming the permanent
        // driver, and every running CPU belongs to its fixed participant
        // set. Rejection violates either that boot invariant or the caller's
        // watched task-context requirement (NMI/unwatched entry is rejected).
        // Either is fatal: an owning replacement may already have withdrawn
        // its old allocation, so this is not a recoverable setup error after
        // publication changed.
        assert!(is_accepted, "prepared RCU wait requires valid admission");
        self.wait.wait_for_completion(&self.completion.completion);
        // Carry the RCU completion ordering to subsequent caller accesses even
        // when completion preceded park or the waiter resumed on another CPU.
        fence(Ordering::SeqCst);
    }
}

/// # Safety
/// `node` is the provenance-preserving zero-offset projection of the independent
/// raw Arc created in prepare. It has not been accepted or invoked. Consume
/// exactly that strong reference; this marker owns no retired protected data.
unsafe fn cancel_grace_period(node: *mut CallbackNode) {
    // SAFETY: preparation transferred this exact Arc reference to the unique
    // unsubmitted handle; no callback can concurrently consume it.
    drop(unsafe { Arc::from_raw(node.cast::<GracePeriodCompletion>()) });
}

/// # Safety
/// `node` must be the provenance-preserving field projection of `node` from a
/// `GracePeriodCompletion` pointer returned by `Arc::into_raw`. Its zero offset
/// is checked at compile time. Invoke exactly once after the GP, consuming that
/// transferred strong reference, outside RCU locks and in a context permitting
/// completion wakeups and Arc cleanup.
unsafe fn complete_grace_period(node: *mut CallbackNode) {
    // SAFETY: repr(C) places node first; submission retained this original Arc
    // reference. The waiter owns a separate reference and may already return
    // while complete() is still notifying it on another CPU.
    let record = unsafe { Arc::from_raw(node.cast::<GracePeriodCompletion>()) };
    record.completion.complete();
}

/// Waits for a future grace period through the boot-owned callback service.
///
/// All fallible preparation precedes submission. Once submitted, signals and
/// spurious wakes cannot end the wait early. No retired object is destroyed by
/// this operation; the caller retains responsibility for its ownership.
/// Completion preparation and waiting do not allocate. The callback's Arc
/// allocation still uses the kernel allocator's fatal OOM policy. This is not
/// a callback barrier.
///
/// # Errors
/// Returns `InvalidInput` outside sleepable task context (including a local
/// read scope), or `BadState` before service readiness. No callback is submitted
/// on error.
///
/// # Safety
/// Call from watched kernel task execution.
/// Every protected access must use this kernel's RCU participation protocol,
/// such as `crate::with_read`, not an independent `krcu-core::TaskReader`.
/// Withdraw every relevant publication route before calling, and retain the
/// old object until successful return. Hold no lock needed by a reader or the
/// service. An error does not authorize reclamation. The boot-owned service
/// and participating CPUs must remain available throughout the wait.
pub unsafe fn synchronize_rcu() -> KResult<()> {
    with_prepared_grace_period(|wait| {
        // SAFETY: the caller guarantees watched task context and publication
        // withdrawal; preparation introduces no intervening locks or readers.
        unsafe { wait.submit_and_wait() }
    })
}

unittest::path_mod_test!(tests, "../ut-tests/rcu_synchronize.rs");

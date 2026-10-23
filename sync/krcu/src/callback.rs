// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! One-shot ownership of existing callback storage, without a wrapper allocation.

use core::{mem::ManuallyDrop, ptr::NonNull};

use kerrno::{KError, KResult};
use krcu_core::CallbackNode;

use super::SERVICE;

/// Checks task context and service readiness before a destructive publication change.
///
/// This does not allocate, wait, submit a callback, reserve service state or pin
/// the caller after return. Check before acquiring writer exclusion or changing
/// publication. A successful check is a snapshot, not a transferable permit.
///
/// Under the permanent boot contract, the driver and waker are never revoked
/// and migration stays within the fixed participating CPU set. The same task
/// may be preempted or migrated before submitting through
/// [`PreparedCallback::submit`]. Prepare all fallible resources before mutation,
/// release writer exclusion before submission, and insert no arbitrary caller
/// code or explicit execution-context/watching transition between commit and
/// submit. Callers still own serialization, withdrawal and callback safety.
///
/// Submission retains its independent checks and original rejection contract,
/// including support for watched hardirq/softirq callers that this task-only
/// preflight rejects. An unexpected rejection after mutation is an invariant
/// failure: retain the retired ownership and diagnose it, rather than canceling
/// protected data or returning an error claiming publication was unchanged.
/// Service teardown or CPU hotplug would require revisiting this protocol.
///
/// # Errors
/// Returns `BadState` if the service is absent (checked first). With a service,
/// invalid context is rejected before checking waker readiness: `InvalidInput`
/// outside sleepable watched task context, including NMI, interrupt/BH or exception
/// context, masked IRQs, disabled preemption, an active read scope, or absent/
/// idle current task storage; `BadState` if the startup waker is not published.
/// No consumer state is changed on either error.
/// Before service startup, the absent-service result requires no current task;
/// once a service exists, the boot contract requires initialized CPU context.
pub fn preflight_callback_submission() -> KResult<()> {
    SERVICE
        .get()
        .ok_or(KError::BadState)?
        .preflight_callback_submission()
}

/// Owns the right to submit one node or release its unsubmitted resources.
///
/// The node remains embedded in caller-provided storage. Dropping this handle
/// calls only the cancellation function, never the post-GP callback. Cancellation
/// is not a grace period and must not reclaim data still visible to readers.
/// There is no Clone, raw-pointer accessor or submission through a shared borrow.
///
/// The submission right cannot be reused after a consuming call:
///
/// ```compile_fail
/// use krcu::PreparedCallback;
/// /// # Safety
/// /// The supplied node meets submission and cancellation contracts. Even
/// /// with that premise, ownership forbids reusing the consumed handle.
/// unsafe fn submit_twice(callback: PreparedCallback) {
///     // SAFETY: assumed by this compile-time-only caller's contract.
///     let _ = unsafe { callback.submit() };
///     // SAFETY: the same assumption cannot restore consumed Rust ownership.
///     let _ = unsafe { callback.submit() };
/// }
/// ```
#[must_use = "dropping an unsubmitted callback releases its preparation, not a grace period"]
pub struct PreparedCallback {
    node: NonNull<CallbackNode>,
    cancel_fn: unsafe fn(*mut CallbackNode),
}

// SAFETY: new requires transferable ownership of the node, both callback code
// paths and their resources. Moving this handle never moves the stable node.
// Shared access is not exposed and NonNull keeps the handle non-Sync.
unsafe impl Send for PreparedCallback {}

impl PreparedCallback {
    /// Adopts one already-prepared node; this does not allocate or submit it.
    /// No running RCU service is needed until submission. This handle does not
    /// move the node; preparing its enclosing storage remains the caller's job.
    ///
    /// # Safety
    /// `node` must preserve its enclosing allocation's provenance and be a live,
    /// initialized, unqueued node at a stable address. Transfer its sole
    /// submission right to this handle; no other path may submit, move or free
    /// it. Storage, callback code and resources must survive until cancellation
    /// or invocation, and permit ownership transfer between tasks/CPUs.
    /// `cancel_fn` must release that ownership exactly once without invoking the
    /// post-GP action. It must be safe before any GP, including after rejected
    /// submission, and in every context where this handle may be dropped. Any
    /// separately protected data must remain independently retained. Both
    /// cancellation and invocation must use this exact node layout/provenance.
    pub unsafe fn new(
        node: NonNull<CallbackNode>,
        cancel_fn: unsafe fn(*mut CallbackNode),
    ) -> Self {
        Self { node, cancel_fn }
    }

    /// Consumes the submission right, or returns it unchanged with the error.
    ///
    /// Success does not imply the callback is still pending: it can finish
    /// before this method returns. Rejection neither cancels nor invokes it.
    ///
    /// # Errors
    /// Returns `InvalidInput` in NMI or unwatched execution, or `BadState` when
    /// the service is unavailable/not ready or this CPU is not a participant.
    /// The returned handle still owns cancellation and may be retried. Neither
    /// rejection nor cancellation establishes permission to reclaim retired data.
    /// Task, hardirq and softirq submissions are supported after initialization;
    /// this operation does not allocate or wait for the grace period.
    ///
    /// # Safety
    /// Withdraw all publication routes to the retired data before submission.
    /// The node's callback and destruction must not block or unwind and must be
    /// valid in softirq context. Retain callback code and referenced resources
    /// through callback return. Hold no scheduler or RCU bookkeeping lock:
    /// without IPIs, progress notification may wake the softirq daemon.
    /// Keep the permanent service and participating CPUs available. This checks
    /// context admission, not arbitrary callback code or data-retirement safety.
    pub unsafe fn submit(self) -> Result<(), (KError, Self)> {
        let Some(service) = SERVICE.get() else {
            return Err((KError::BadState, self));
        };
        // Suppress cancellation before making the node visible to the executor,
        // which may free its storage before submit_callback returns.
        let prepared = ManuallyDrop::new(self);
        // SAFETY: new retained a unique stable node; the caller supplies the
        // withdrawal and execution-context obligations. No cancellation can
        // race this transfer, and rejection leaves the node untouched.
        match unsafe { service.submit_callback(prepared.node.as_ptr()) } {
            Ok(()) => Ok(()),
            Err(error) => Err((error, ManuallyDrop::into_inner(prepared))),
        }
    }
}

impl Drop for PreparedCallback {
    fn drop(&mut self) {
        // SAFETY: accepted submissions suppress this destructor. Otherwise new
        // retained the unique cancellation owner, and rejection preserves it.
        // Cancellation owns its own cleanup policy; no node access follows it.
        unsafe { (self.cancel_fn)(self.node.as_ptr()) };
    }
}

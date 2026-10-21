// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! One-shot ownership of existing callback storage, without a wrapper allocation.

use core::{mem::ManuallyDrop, ptr::NonNull};

use kerrno::KError;
use krcu_core::CallbackNode;

use super::GracePeriodTask;

/// Owns the right to submit one node or release its unsubmitted resources.
///
/// The node remains embedded in caller-provided storage. Dropping this handle
/// calls only the cancellation function, never the post-GP callback. Cancellation
/// is not a grace period and must not reclaim data still visible to readers.
/// There is no Clone, raw-pointer accessor or submission through a shared borrow.
#[must_use = "dropping an unsubmitted callback releases its preparation, not a grace period"]
pub(super) struct PreparedCallback {
    service: &'static GracePeriodTask,
    node: NonNull<CallbackNode>,
    cancel_fn: unsafe fn(*mut CallbackNode),
}

// SAFETY: new requires transferable ownership of the node, both callback code
// paths and their resources. Moving this handle never moves the stable node.
// Shared access is not exposed and NonNull keeps the handle non-Sync.
unsafe impl Send for PreparedCallback {}

impl PreparedCallback {
    /// Adopts one already-prepared node; this does not allocate or submit it.
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
    pub(super) unsafe fn new(
        service: &'static GracePeriodTask,
        node: NonNull<CallbackNode>,
        cancel_fn: unsafe fn(*mut CallbackNode),
    ) -> Self {
        Self {
            service,
            node,
            cancel_fn,
        }
    }

    /// Consumes the submission right, or returns it unchanged with the error.
    ///
    /// Success does not imply the callback is still pending: it can finish
    /// before this method returns. Rejection neither cancels nor invokes it.
    ///
    /// # Safety
    /// Meet `GracePeriodTask::submit_callback`'s withdrawal, nonblocking
    /// invocation/destruction and lock-order requirements. This checks context
    /// admission, not the safety of arbitrary callback code or retired data.
    pub(super) unsafe fn submit(self) -> Result<(), (KError, Self)> {
        // Suppress cancellation before making the node visible to the executor,
        // which may free its storage before submit_callback returns.
        let prepared = ManuallyDrop::new(self);
        // SAFETY: new retained a unique stable node; the caller supplies the
        // withdrawal and execution-context obligations. No cancellation can
        // race this transfer, and rejection leaves the node untouched.
        match unsafe { prepared.service.submit_callback(prepared.node.as_ptr()) } {
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

unittest::path_mod_test!(tests, "rcu_callback.rs");

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-request waiting and per-device completion contracts.

use alloc::{boxed::Box, sync::Arc};

use crate::DriverResult;

/// Driver operations for reclaiming a block device's completed I/O requests.
///
/// Implemented by the device that owns the hardware request queue, not by a
/// separate callback object. The host decides when to invoke these operations;
/// the driver identifies completed requests, publishes results and notifies
/// their callers. This interface does not submit I/O or own host dispatch state.
///
/// Each device has one host completion-processing state. Its activation owner
/// retains a strong reference until completion processing has stopped.
pub trait BlockCompletionOperations: Send + Sync {
    /// Reclaims a finite batch of completed requests and notifies their callers.
    ///
    /// May run with IRQs enabled in non-sleepable context. Release device locks
    /// before notifying callers. Do not submit more I/O, wait for unfinished
    /// requests, or allocate queue bookkeeping here.
    /// The host serializes invocations for the same device through its single
    /// completion-processing state, including the notification part of a pass.
    fn process_completed_requests(&self);
}

/// Independently owned notification sources for one block transaction.
///
/// Methods must be non-sleeping and IRQ-safe. Invoke them after releasing
/// device and registry locks. Implementations must not retain request, data
/// buffer or device pointers; a signal may outlive the originating call.
pub trait BlockSignals: Send + Sync {
    /// Adds one admission hint, remembered even if the caller is not waiting.
    ///
    /// A hint does not grant submission rights; the caller must recheck FIFO
    /// ownership and descriptor availability under device protection.
    fn notify_admission(&self);

    /// Permanently signals terminal completion, independently of admission.
    ///
    /// Publish the request result and relinquish device buffer ownership first.
    /// Repeated notification must not reset completion or create a new result.
    fn notify_completion(&self);
}

/// Task-local waiting half of a prepared block transaction.
///
/// Only the preparing task may call these methods, without locks that forbid
/// sleeping. The waiter stays outside the shared request node. Notification
/// does not itself own the driver's request state or result.
pub trait BlockWaiter {
    /// Returns a shareable handle to this waiter's existing notification state.
    ///
    /// Cloning the handle must not allocate or create another notification
    /// source. It may outlive this task-bound waiter.
    fn signals(&self) -> Arc<dyn BlockSignals>;

    /// Consumes an admission hint, blocking if necessary; pre-submit only.
    ///
    /// # Errors
    /// A registration failure may be returned before submission. The caller
    /// must unlink its pending request and hand admission to the next caller.
    fn wait_admission(&mut self) -> DriverResult<()>;

    /// Waits non-interruptibly for the permanent terminal notification.
    ///
    /// Must not register a new wait, allocate a waker or return a wait error.
    /// Spurious task wakes must not let this method return before completion.
    fn wait_completion(&mut self);
}

/// Prepares task-local waiting resources before FIFO publication.
///
/// Injected by the host; requires IRQ-enabled, sleepable task context.
/// Obtain the shared notification state through [`BlockWaiter::signals`].
///
/// # Errors
/// Returns InvalidInput for invalid context and a driver error for a
/// recoverable preparation failure, before any device ownership starts.
/// Infallible allocation follows the host kernel's OOM policy.
pub type PrepareBlockWait = fn() -> DriverResult<Box<dyn BlockWaiter>>;

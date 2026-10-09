// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Threaded execution authority for safely publishing kernel-lifetime Work.

use alloc::sync::Arc;
use core::pin::Pin;

use kcpu_id_map::LogicalCpuId;

use crate::{
    ExecutionKind, QueueConfig, QueueCreateError, SubmitError, WaitError, Work, WorkQueue,
};

/// An independently controlled queue bound exclusively to normal worker tasks.
///
/// Unlike an arbitrary WorkQueue, this type certifies sleepable callback execution.
/// Clones share one queue and its shutdown state; no conversion from an arbitrary
/// queue or access to its underlying handle is provided. Drop does not drain.
#[derive(Clone)]
pub struct NormalQueue(Arc<WorkQueue>);

impl NormalQueue {
    pub(crate) fn work_queue(&self) -> &Arc<WorkQueue> {
        &self.0
    }

    /// Prepares a named normal queue in sleepable context after service startup.
    /// Preparation may allocate; later static submissions do not.
    ///
    /// # Errors
    /// Returns the context, readiness or CPU error from queue creation.
    ///
    /// # Panics
    /// Propagates fatal allocation failures and provider invariant violations.
    pub fn new(name: &str, config: QueueConfig) -> Result<Self, QueueCreateError> {
        crate::create_queue(name, config, ExecutionKind::Normal).map(Self)
    }

    /// Submits kernel-lifetime Work for asynchronous execution in a worker task.
    ///
    /// The static lifetime guarantees valid storage and captures; callers need
    /// neither manual pinning nor a release-time drain to keep them alive. Routing
    /// follows QueueConfig; an unfinished Work keeps its original queue/CPU owner.
    /// Valid in task/BH/hard IRQ context, not NMI. Does not allocate or sleep.
    /// Callback dependency cycles may still deadlock; safe does not promise progress.
    ///
    /// # Errors
    /// Returns ordinary admission errors, including duplicate, closed and ownership
    /// rejection, without accepting a new occurrence.
    ///
    /// # Panics
    /// Propagates broken accounting or execution-provider invariants.
    ///
    /// ```compile_fail
    /// fn local_work_is_not_static(queue: &kworkng::NormalQueue) {
    ///     let work = kworkng::Work::with_callback(|| {});
    ///     queue.queue_static_work(&work, None).unwrap();
    /// }
    /// ```
    pub fn queue_static_work<H: Fn() + Send + Sync + 'static>(
        &self,
        work: &'static Work<H>,
        target: Option<LogicalCpuId>,
    ) -> Result<LogicalCpuId, SubmitError> {
        // SAFETY: a shared static borrow cannot be moved or destroyed by safe
        // callers. Private construction binds only normal, kernel-lifetime
        // providers, so arbitrary safe callbacks execute in task context. Work
        // retains the queue during asynchronous use; no Drop/drain is needed
        // to keep this permanently retained Work and its captures alive.
        let work = unsafe { Pin::new_unchecked(work) };
        // SAFETY: This static work and its captures remain live forever; the normal
        // queue runs callbacks in task context and retains its provider during use.
        unsafe { self.0.queue_work(work, target) }
    }

    /// Waits for the queue's captured ordinary-work batch without closing admission.
    /// Requires sleepable context; concurrent later submissions are not joined.
    ///
    /// # Errors
    /// Returns context, optional self-wait or wait-preparation errors.
    ///
    /// # Panics
    /// Propagates broken completion/accounting invariants and fatal allocation.
    pub fn flush(&self) -> Result<(), WaitError> {
        self.0.flush()
    }

    /// Permanently closes admission and drains accepted work in sleepable context.
    /// All clones observe closure. Before closing, stop producers and shut down
    /// any DelayedWorkHandle submitted here; timer-only work is not part of drain.
    ///
    /// # Errors
    /// Returns context, optional self-wait or wait-preparation errors before closure.
    ///
    /// # Panics
    /// Propagates broken completion/accounting invariants and fatal allocation.
    pub fn close_and_drain(&self) -> Result<(), WaitError> {
        self.0.close_and_drain()
    }
}

/// Submits static Work to the kernel-owned default normal queue.
///
/// Callback execution is in worker task context, never inline or in BH context.
/// Submission is allocation-free in task/BH/hard IRQ context, not NMI. Static
/// storage and captures cannot expire; caller pinning and teardown are unnecessary
/// for memory safety. This does not prevent logical callback dependency deadlocks.
///
/// # Errors
/// Returns NotReady before publication when services are unavailable, or the
/// ordinary admission error without accepting a new occurrence.
///
/// # Panics
/// Propagates broken accounting or execution-provider invariants.
///
/// ```no_run
/// fn submit(work: &'static kworkng::Work<fn()>) {
///     kworkng::queue_static_work(work, None).unwrap();
/// }
/// ```
///
/// ```compile_fail
/// let work = kworkng::Work::with_callback(|| {});
/// kworkng::queue_static_work(&work, None).unwrap();
/// ```
pub fn queue_static_work<H: Fn() + Send + Sync + 'static>(
    work: &'static Work<H>,
    target: Option<LogicalCpuId>,
) -> Result<LogicalCpuId, crate::ServiceError> {
    // SAFETY: static shared storage/captures remain stable and live forever.
    // The fixed Normal selector resolves initialized kernel-lifetime providers
    // and cannot execute this callback under the BH execution contract.
    let work = unsafe { Pin::new_unchecked(work) };
    // SAFETY: This static work and its captures remain live forever; the normal
    // queue runs callbacks in task context and retains its provider during use.
    unsafe { crate::DefaultQueue::Normal.queue_work(work, target) }
}

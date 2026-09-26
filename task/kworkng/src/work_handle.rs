// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared ownership for ordinary Work with asynchronous final reclamation.

use alloc::sync::Arc;

use kcpu_id_map::LogicalCpuId;

use crate::{HandleCreateError, NormalQueue, SubmitError, WaitError, Work, owned::Owner};

/// A cloneable ordinary Work owner with safe, allocation-free publication.
///
/// Callbacks own static captures and execute only in normal worker tasks. Last
/// handle Drop does not wait: a prepared normal Work drains callbacks and destroys
/// captures in task context. Drop may revoke pending work, not guarantee execution.
/// Use shutdown for synchronous closure. Forgetting/cycling handles may leak but
/// cannot invalidate storage; callback dependency deadlocks remain caller errors.
pub struct WorkHandle<H: Fn() + Send + Sync + 'static>(Arc<Owner<Work<H>>>);

impl<H: Fn() + Send + Sync + 'static> Clone for WorkHandle<H> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<H: Fn() + Send + Sync + 'static> WorkHandle<H> {
    /// Prepares owned storage and eventual reclamation in sleepable context.
    ///
    /// # Errors
    /// Rejects invalid context or unavailable normal services before preparation.
    /// On rejection the supplied callback is dropped in the calling context.
    ///
    /// # Panics
    /// Propagates fatal allocation failures.
    pub fn new(callback: H) -> Result<Self, HandleCreateError> {
        Owner::new(|| Work::with_callback(callback)).map(Self)
    }

    /// Submits to a normal queue without allocation or sleeping, including in IRQ.
    /// NMI is unsupported. Clones share Work identity and permanent closure state.
    ///
    /// # Errors
    /// Returns Closed after handle shutdown or ordinary queue admission rejection.
    ///
    /// # Panics
    /// Propagates provider and accounting invariant failures.
    pub fn queue(
        &self,
        queue: &NormalQueue,
        target: Option<LogicalCpuId>,
    ) -> Result<LogicalCpuId, SubmitError> {
        self.0.submit(|work| {
            // SAFETY: this Owner retains pinned storage through admission;
            // final Drop transfers it to normal-context drain before destruction.
            // NormalQueue's private constructor certifies callback context.
            unsafe { queue.work_queue().queue_work(work, target) }
        })?
    }

    /// Submits to the permanent default normal queue with the same contract as queue.
    ///
    /// # Errors
    /// Returns Closed after handle shutdown or ordinary admission rejection.
    ///
    /// # Panics
    /// Propagates provider and accounting invariant failures.
    pub fn queue_default(&self, target: Option<LogicalCpuId>) -> Result<LogicalCpuId, SubmitError> {
        self.0.submit(|work| {
            // SAFETY: same Owner retention and terminal drain as queue; this
            // normal provider was retained during successful preparation.
            unsafe { self.0.queue().queue_work(work, target) }
        })?
    }

    /// Revokes pending work without waiting; running callbacks may continue.
    /// IRQ-compatible, allocation-free, not a permanent close or release barrier.
    ///
    /// # Panics
    /// Propagates cleanup/accounting invariant failures.
    pub fn cancel_pending(&self) -> bool {
        self.0.work().cancel_pending()
    }

    /// Cancels and drains captured work without permanently closing admission.
    /// Requires sleepable context and callback progress; racing producers may
    /// submit again after the temporary disable ends.
    ///
    /// # Errors
    /// Returns context, optional self-wait or wait-preparation errors.
    ///
    /// # Panics
    /// Propagates cleanup invariants and fatal allocation failures.
    pub fn cancel_sync(&self) -> Result<bool, WaitError> {
        self.0.work().cancel_sync()
    }

    /// Waits for captured work in sleepable context without closing admission.
    ///
    /// # Errors
    /// Returns context, optional self-wait or wait-preparation errors.
    ///
    /// # Panics
    /// Propagates completion invariant or fatal allocation failures.
    pub fn flush(&self) -> Result<(), WaitError> {
        self.0.work().flush()
    }

    /// Permanently closes every clone, then synchronously drains in task context.
    /// Invalid context is rejected before closure. Other drain errors leave the
    /// gate closed; retry shutdown to complete drain. Never call from this callback
    /// or while preventing its progress. Last Drop still reclaims safely on error.
    ///
    /// # Errors
    /// Returns invalid context, optional self-wait or drain preparation errors.
    ///
    /// # Panics
    /// Propagates cleanup invariants and fatal allocation failures.
    pub fn shutdown(&self) -> Result<(), WaitError> {
        self.0.shutdown()
    }
}

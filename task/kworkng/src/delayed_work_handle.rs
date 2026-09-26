// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Owned delayed Work reusing the common admission and reclamation lifetime.

use alloc::sync::Arc;

use kcpu_id_map::LogicalCpuId;
use ktime_types::TimeSpan;

use crate::{
    ArmError, DelayedFlushError, DelayedWork, HandleCreateError, NormalQueue, WaitError,
    owned::Owner,
};

/// A cloneable delayed Work owner with safe, allocation-free prepared submission.
///
/// Callbacks execute in normal tasks. Last handle Drop asynchronously cancels and
/// drains both ordinary Work and physical timer delivery before destroying captures
/// in task context. It does not guarantee pending callbacks execute. Use shutdown
/// before closing a private queue. Forgotten/cyclic handles can leak; callback
/// dependency deadlocks remain caller errors. NMI is unsupported.
pub struct DelayedWorkHandle<H: Fn() + Send + Sync + 'static>(Arc<Owner<DelayedWork<H>>>);

impl<H: Fn() + Send + Sync + 'static> Clone for DelayedWorkHandle<H> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<H: Fn() + Send + Sync + 'static> DelayedWorkHandle<H> {
    /// Prepares stable Work, timer and reclamation storage in sleepable context.
    ///
    /// # Errors
    /// Rejects invalid context or unavailable normal services before preparation.
    /// Rejected callbacks are dropped in the calling context.
    ///
    /// # Panics
    /// Propagates timer preparation invariants and fatal allocation failures.
    pub fn new(callback: H) -> Result<Self, HandleCreateError> {
        Owner::new(|| DelayedWork::new(callback)).map(Self)
    }

    /// Queues after delay on a normal queue; zero delay queues without a timer tick.
    /// IRQ-compatible and allocation-free. None uses queue policy/unfinished owner;
    /// an explicit target must satisfy existing queue and Work ownership rules.
    ///
    /// # Errors
    /// Returns Admission(Closed) after shutdown, or queue/timer validation errors.
    /// Rejection preserves the existing arrangement, apart from concurrent changes.
    ///
    /// # Panics
    /// Propagates timer, routing and accounting invariant failures.
    pub fn queue(
        &self,
        queue: &NormalQueue,
        delay: TimeSpan,
        target: Option<LogicalCpuId>,
    ) -> Result<LogicalCpuId, ArmError> {
        self.0
            .submit(|work| {
                // SAFETY: Owner retains pinned Work through both physical timer and
                // ordinary drain. NormalQueue certifies task-context execution.
                unsafe { queue.work_queue().queue_delayed_work(work, delay, target) }
            })
            .map_err(ArmError::Admission)?
    }

    /// Queues after delay on the permanent default normal queue, including in IRQ.
    /// Uses the same delay/routing/no-allocation contract as queue.
    ///
    /// # Errors
    /// Returns closure, admission or timer validation errors without replacing work.
    ///
    /// # Panics
    /// Propagates timer, routing and accounting invariant failures.
    pub fn queue_default(
        &self,
        delay: TimeSpan,
        target: Option<LogicalCpuId>,
    ) -> Result<LogicalCpuId, ArmError> {
        self.0
            .submit(|work| {
                // SAFETY: same retention as queue, with the prepared normal provider.
                unsafe { self.0.queue().queue_delayed_work(work, delay, target) }
            })
            .map_err(ArmError::Admission)?
    }

    /// Replaces pending delay, or queues when none is pending, without allocation.
    /// Running work is unchanged; unfinished queue/CPU ownership is preserved.
    /// Zero makes pending work ready without inline execution. IRQ-compatible.
    ///
    /// # Errors
    /// Returns closure, ownership, admission or timer errors; rejection preserves
    /// the arrangement except for independent concurrent operations.
    ///
    /// # Panics
    /// Propagates timer, routing and accounting invariant failures.
    pub fn modify(
        &self,
        queue: &NormalQueue,
        delay: TimeSpan,
        target: Option<LogicalCpuId>,
    ) -> Result<LogicalCpuId, ArmError> {
        self.0
            .submit(|work| {
                // SAFETY: the gated Owner retains the fixed timer/Work pair through
                // final drain; NormalQueue restricts the eventual callback context.
                unsafe { queue.work_queue().modify_delayed_work(work, delay, target) }
            })
            .map_err(ArmError::Admission)?
    }

    /// Modifies or queues on the default normal queue with modify's contract.
    ///
    /// # Errors
    /// Returns closure, ownership, admission or timer validation errors.
    ///
    /// # Panics
    /// Propagates timer, routing and accounting invariant failures.
    pub fn modify_default(
        &self,
        delay: TimeSpan,
        target: Option<LogicalCpuId>,
    ) -> Result<LogicalCpuId, ArmError> {
        self.0
            .submit(|work| {
                // SAFETY: same gated lifetime as modify, using retained normal service.
                unsafe { self.0.queue().modify_delayed_work(work, delay, target) }
            })
            .map_err(ArmError::Admission)?
    }

    /// Revokes pending work without waiting for callbacks or physical delivery.
    /// IRQ-compatible and allocation-free; does not close future admission.
    ///
    /// # Panics
    /// Propagates timer, cleanup and accounting invariant failures.
    pub fn cancel_pending(&self) -> bool {
        self.0.work().cancel_pending()
    }

    /// Drains ordinary execution and physical delivery without permanent closure.
    /// Sleepable context only; racing producers may submit after temporary disable.
    /// Never call from the callback or while preventing its progress.
    ///
    /// # Errors
    /// Returns context, optional self-wait or wait-preparation errors.
    ///
    /// # Panics
    /// Propagates cleanup invariants and fatal preparation failures.
    pub fn cancel_sync(&self) -> Result<bool, WaitError> {
        self.0.work().cancel_sync()
    }

    /// Expedites and waits for captured work in sleepable context.
    /// Does not close admission or follow later retiming; concurrent modification
    /// can end the captured obligation without executing the callback.
    ///
    /// # Errors
    /// Returns wait preparation, context, self-wait or transfer errors.
    ///
    /// # Panics
    /// Propagates timer/accounting invariants and fatal preparation failures.
    pub fn flush(&self) -> Result<(), DelayedFlushError> {
        self.0.work().flush()
    }

    /// Permanently closes all clones, then drains Work and timer in task context.
    /// Invalid context leaves admission open; other errors leave it closed and
    /// permit retry. Do not call from this callback or prevent its progress.
    /// Last Drop retains storage for eventual reclamation even after an error.
    ///
    /// # Errors
    /// Returns context, optional self-wait or drain preparation errors.
    ///
    /// # Panics
    /// Propagates cleanup invariants and fatal preparation failures.
    pub fn shutdown(&self) -> Result<(), WaitError> {
        self.0.shutdown()
    }
}

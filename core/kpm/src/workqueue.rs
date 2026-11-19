// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Caller-selected reversible queue participation, without a global registry.

use alloc::vec::Vec;

use ktime_types::TimeSpan;
use kworkng::{FrozenQueue, NormalQueue, QueueFreezeCancellation, QueueFreezeError};

/// Private normal queues whose callbacks must be stopped before device suspend.
///
/// Select dependent queues before providers; previously claimed callbacks must
/// finish without frozen tasks or pending work on an earlier paused queue.
/// Exempt their existing workers explicitly through [`crate::SuspendTasks`].
/// Keep PM, RTC and recovery dependencies off these queues. This selection does
/// not cover shared defaults, unlisted queues, BH/IRQ work or timer delivery.
pub struct SuspendWorkqueues<'a> {
    /// Explicit queue participation; clones of one queue share pause ownership.
    /// Repeating a queue fails Busy at its later position and rolls back the prefix.
    pub queues: &'a [NormalQueue],
    /// Maximum advancing-MONOTONIC wait for each queue's claimed callbacks.
    /// The total preparation budget is not limited to this duration.
    pub timeout: TimeSpan,
    /// One-shot queue-phase cancellation, prepared before entering PM.
    /// Also signal the task-phase handle when cancelling the whole preparation.
    pub cancellation: &'a QueueFreezeCancellation,
}

/// Failed queue preparation, retaining its position in the supplied list.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkqueueFreezeError {
    /// Zero-based index of the first queue that could not freeze.
    pub index: usize,
    /// Underlying context, timer, closed, busy, cancellation or timeout failure.
    pub error: QueueFreezeError,
}

/// Owns one PM attempt's queue policy and acquired pause guards.
/// Successful guards survive a later failure until platform cleanup completes.
pub(crate) struct WorkqueueFreezeSession<'a> {
    policy: Option<SuspendWorkqueues<'a>>,
    frozen: Vec<FrozenQueue>,
}

impl<'a> WorkqueueFreezeSession<'a> {
    pub(crate) fn empty() -> Self {
        Self {
            policy: None,
            frozen: Vec::new(),
        }
    }

    pub(crate) fn new(policy: SuspendWorkqueues<'a>) -> Self {
        // Reserve before platform preparation and task freezing. Each queue
        // still prepares its own timeout notification while time is advancing.
        let frozen = Vec::with_capacity(policy.queues.len());
        Self {
            policy: Some(policy),
            frozen,
        }
    }

    pub(crate) fn freeze(&mut self) -> Result<(), WorkqueueFreezeError> {
        let Some(policy) = &self.policy else {
            return Ok(());
        };
        for (index, queue) in policy.queues.iter().enumerate() {
            let guard = queue
                .freeze(policy.timeout, policy.cancellation)
                .map_err(|error| WorkqueueFreezeError { index, error })?;
            self.frozen.push(guard);
        }
        // On failure the successful prefix remains owned until platform finish.
        Ok(())
    }

    pub(crate) fn thaw(&mut self) {
        while let Some(guard) = self.frozen.pop() {
            drop(guard);
        }
    }
}

unittest::path_mod_test!(tests, "../ut-tests/workqueue.rs");

/// Owns task admission and selected queue pauses while CPUs remain online.
///
/// Drop resumes queues before tasks and deferred activations. Keep the guard on
/// its preparing task until resources used by resumed callbacks are usable.
/// This is software preparation, not CPU, IRQ, DMA or platform suspension.
#[must_use = "retain until callback and task resources are safe to resume"]
pub struct FrozenSystemWorkqueues<'a> {
    workqueues: WorkqueueFreezeSession<'a>,
    _tasks: ktask::freezer::SystemFrozenTasks,
    _admission: crate::Admission<'static>,
}

impl Drop for FrozenSystemWorkqueues<'_> {
    fn drop(&mut self) {
        self.workqueues.thaw();
    }
}

/// Freezes admitted tasks and selected private queues across initialized CPUs.
///
/// The coordinator keeps IRQs, clocks and all CPUs running. Tasks freeze first;
/// explicitly exempt every queue worker and other infrastructure needed for
/// progress. Already claimed callbacks must finish without frozen tasks or work
/// held by an earlier queue. There is no automatic worker discovery/exemption.
/// New task activation remains deferred until the returned guard is dropped.
///
/// Recover any test-owned or subsystem resources before dropping the guard.
/// Do not freeze clocks, power devices off, or enter firmware based on this
/// guard: it does not exclude IRQ/BH/DMA, idle or exempt execution. The separate
/// platform-suspend entry still requires one present CPU.
///
/// # Errors
/// Rejects invalid context or a concurrent transaction before freezing. Task
/// failure returns [`crate::SuspendError::TaskFreeze`]. Queue failure returns
/// [`crate::SuspendError::WorkqueueFreeze`] after reverse queue release, task
/// thaw and reopening activation admission. No platform callback is performed.
///
/// # Panics
/// Propagates allocation and initialized scheduler/queue invariant failures.
pub fn freeze_system_workqueues<'a>(
    tasks: crate::SuspendTasks<'_>,
    workqueues: SuspendWorkqueues<'a>,
) -> Result<FrozenSystemWorkqueues<'a>, crate::SuspendError> {
    if !ktask::is_sleepable_context() {
        return Err(crate::SuspendError::InvalidContext);
    }
    let admission = crate::Admission::acquire(&crate::TRANSACTION_ACTIVE)?;
    let mut session = WorkqueueFreezeSession::new(workqueues);
    let frozen_tasks =
        ktask::freezer::freeze_system_tasks(tasks.exempt, tasks.timeout, tasks.cancellation)
            .map_err(crate::SuspendError::TaskFreeze)?;
    if let Err(error) = session.freeze() {
        session.thaw();
        return Err(crate::SuspendError::WorkqueueFreeze(error));
    }
    Ok(FrozenSystemWorkqueues {
        workqueues: session,
        _tasks: frozen_tasks,
        _admission: admission,
    })
}

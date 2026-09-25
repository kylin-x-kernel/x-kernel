// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Ownership-aware admission of prepared external Work, without heap submissions.

use alloc::sync::Arc;

use kcpu_id_map::LogicalCpuId;

use super::{Queue, config::CpuPolicy, partition::AdmissionResult};
use crate::work::Work;

/// Rejected admission leaves the existing Work and routing sequence unchanged.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubmitError {
    /// At least one independent disable owner blocks new submission.
    Disabled,
    /// The Queue has permanently closed admission.
    Closed,
    /// The selected CPU is not allowed by this Queue.
    InvalidCpu(LogicalCpuId),
    /// An unfinished occurrence belongs to another Queue or explicit CPU.
    OwnerConflict,
    /// Work already has an unclaimed occurrence.
    AlreadyQueued,
    /// The original batch cannot represent one more outstanding occurrence.
    CountOverflow,
}

impl Queue {
    /// Selects a CPU and commits one prepared occurrence without allocation.
    ///
    /// Implicit submission reuses an unfinished owner; otherwise selection follows
    /// the configured policy. The private current_cpu provider must sample the
    /// kernel's current logical CPU at invocation, not cache the entry CPU. It runs
    /// outside locks; a changed owner causes selection to retry. Explicit targets
    /// and owner reuse never advance round-robin. Execution is never inline here.
    ///
    /// # Safety
    /// Keep Work and its captures stable, live and shared through every accepted
    /// occurrence and concurrent access. Stop/join producers and drain before
    /// freeing external storage; neither Queue nor its lists own Work.
    ///
    /// # Errors
    /// Returns a distinguishable rejection without publishing new work or advancing
    /// routing. Concurrent failure reasons have no promised priority.
    pub(crate) unsafe fn submit<H: Fn() + Send + Sync>(
        self: &Arc<Self>,
        work: &Work<Arc<Self>, H>,
        target: Option<LogicalCpuId>,
        current_cpu: impl Fn() -> LogicalCpuId,
    ) -> Result<LogicalCpuId, SubmitError> {
        loop {
            if self.admission_color().is_none() {
                return Err(SubmitError::Closed);
            }
            // Snapshot only: never acquire a partition while holding Work's lock.
            let owner = work.state.lock().owner_partition();
            let resolved_target = self.resolve_target(owner.as_ref(), target)?;
            let cpu = self
                .select_cpu(resolved_target, &current_cpu)
                .map_err(SubmitError::InvalidCpu)?;
            let partition = self
                .partition(cpu)
                .expect("selection validated allowed CPU");
            // SAFETY: caller preserves external storage; this is our owned partition.
            match unsafe { partition.accept_from_queue(self, work, owner.as_ref()) } {
                AdmissionResult::Accepted(kick) => {
                    if resolved_target.is_none() && self.config.policy() == CpuPolicy::RoundRobin {
                        self.advance_round_robin();
                    }
                    if let Some(kick) = kick {
                        kick.wake_by_ref();
                    }
                    return Ok(cpu);
                }
                AdmissionResult::Retry => continue,
                AdmissionResult::Rejected(error) => return Err(error),
            }
        }
    }
}

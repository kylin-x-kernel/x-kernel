// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Queue-owned routing for prepared delayed admission and modification.

use alloc::sync::Arc;

use kcpu_id_map::LogicalCpuId;
use ktime_types::MonotonicInstant;

use super::{Queue, SubmitError, config::CpuPolicy};
use crate::work::{Work, delayed::ArmError};

impl Queue {
    /// Modifies pending work or retries fresh admission after claim/cancel wins.
    /// None means immediate ordinary queueing. No cancellation gap or replacement
    /// resource is introduced. Each attempt commits under existing partition/Work
    /// protection; competing admission retries against its now-current pending slot.
    ///
    /// # Safety
    /// Same stable Work/timer binding, initialized service, callback context and
    /// current_cpu sampling obligations as arm_delayed and ordinary submit.
    ///
    /// # Errors
    /// Rejection preserves the old arrangement and routing cursor. Only an
    /// independently accepted competing occurrence can cause an internal retry.
    ///
    /// # Panics
    /// Propagates underlying queue, timer and observer invariant failures.
    pub(crate) unsafe fn modify_delayed<
        H: Fn() + Send + Sync,
        TH: ktask::TimerHandler + 'static,
    >(
        self: &Arc<Self>,
        work: &Work<Arc<Self>, H>,
        timer: &ktask::Timer<TH>,
        target: Option<LogicalCpuId>,
        deadline: Option<MonotonicInstant>,
        current_cpu: impl Fn() -> LogicalCpuId,
    ) -> Result<LogicalCpuId, ArmError> {
        loop {
            if self.admission_color().is_none() {
                return Err(ArmError::Admission(SubmitError::Closed));
            }
            let owner = work.state.lock().owner_partition();
            self.resolve_target(owner.as_ref(), target)
                .map_err(ArmError::Admission)?;
            if let Some(owner) = owner {
                // SAFETY: routing validated Queue/CPU identity. The transaction
                // rechecks pending ownership; caller retains the matching pair.
                if unsafe { owner.retime_pending(work, timer, deadline) }? {
                    return Ok(owner.cpu_id());
                }
            }
            let result = match deadline {
                // SAFETY: same pinned Work/bound timer and sampling contract;
                // fresh admission revalidates any owner acquired concurrently.
                Some(deadline) => unsafe {
                    self.arm_delayed(work, timer, target, deadline, &current_cpu)
                },
                None => {
                    // SAFETY: caller retains Work through ordinary execution/drain.
                    unsafe { self.submit(work, target, &current_cpu) }.map_err(ArmError::Admission)
                }
            };
            match result {
                Err(ArmError::Admission(SubmitError::AlreadyQueued)) => continue,
                result => return result,
            }
        }
    }

    /// Resolves and commits a prepared timer using ordinary Work ownership rules.
    /// The absolute deadline is preserved across owner-change retries. CPU sampling
    /// runs outside locks and is used only for fresh CurrentCpu selection. Timer
    /// IRQ placement is independent of the selected ordinary execution partition.
    ///
    /// # Safety
    /// Work and the matching bound timer must remain shared, stable and live through
    /// all delivery, execution and API accesses. The caller provides initialized
    /// timer service and an IRQ-compatible handler; current_cpu must sample the
    /// actual logical CPU without allocating. Stop producers and drain before free.
    ///
    /// # Errors
    /// Rejection preserves placement and the RR cursor. Failed attempts never arm
    /// a timer; changed owner snapshots retry before physical programming.
    ///
    /// # Panics
    /// Panics if selection or initialized timer-service invariants are violated.
    pub(crate) unsafe fn arm_delayed<H: Fn() + Send + Sync, TH: ktask::TimerHandler + 'static>(
        self: &Arc<Self>,
        work: &Work<Arc<Self>, H>,
        timer: &ktask::Timer<TH>,
        target: Option<LogicalCpuId>,
        deadline: MonotonicInstant,
        current_cpu: impl Fn() -> LogicalCpuId,
    ) -> Result<LogicalCpuId, ArmError> {
        loop {
            if self.admission_color().is_none() {
                return Err(ArmError::Admission(SubmitError::Closed));
            }
            let owner = work.state.lock().owner_partition();
            let resolved = self
                .resolve_target(owner.as_ref(), target)
                .map_err(ArmError::Admission)?;
            let cpu = self
                .select_cpu(resolved, &current_cpu)
                .map_err(|cpu| ArmError::Admission(SubmitError::InvalidCpu(cpu)))?;
            let partition = self
                .partition(cpu)
                .expect("selection validated allowed CPU");
            // SAFETY: this Queue owns the selected partition. Caller retains the
            // concrete Work/bound timer pair; the transaction rechecks owner before start.
            if unsafe { partition.arm_delayed(self, work, timer, deadline, owner.as_ref()) }? {
                if resolved.is_none() && self.config.policy() == CpuPolicy::RoundRobin {
                    self.advance_round_robin();
                }
                return Ok(cpu);
            }
        }
    }
}

#[cfg(unittest)]
mod tests;

#[cfg(unittest)]
mod modify_tests;

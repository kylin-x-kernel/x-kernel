// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Queue-owned CPU selection and post-acceptance round-robin advancement.
//! Selecting a CPU has no effect. Ordinary and delayed admission advance only after successful
//! automatic RR admission; explicit targets and unfinished owner reuse bypass it.
//!
//! No queue-wide spinlock is needed. Concurrent calls may select the same CPU;
//! every accepted automatic selection still advances once. The cursor only
//! distributes targets; it publishes no Work, list, closure or progress state.

use alloc::sync::Arc;
use core::sync::atomic::Ordering;

use kcpu_id_map::LogicalCpuId;

use super::{Queue, SubmitError, config::CpuPolicy, partition::QueuePartition};

impl Queue {
    /// Resolves unfinished ownership before policy selection. None means fresh
    /// automatic selection, so only that case may consume a round-robin turn.
    /// The admission transaction must revalidate the same owner snapshot.
    pub(super) fn resolve_target(
        &self,
        owner: Option<&Arc<QueuePartition>>,
        target: Option<LogicalCpuId>,
    ) -> Result<Option<LogicalCpuId>, SubmitError> {
        let Some(owner) = owner else {
            return Ok(target);
        };
        if !self
            .partition(owner.cpu_id())
            .is_some_and(|p| Arc::ptr_eq(p, owner))
            || target.is_some_and(|cpu| cpu != owner.cpu_id())
        {
            return Err(SubmitError::OwnerConflict);
        }
        Ok(Some(owner.cpu_id()))
    }

    /// Samples current CPU only for implicit CurrentCpu selection. The caller
    /// must sample through the kernel current-CPU API, not a cached entry CPU.
    /// Returns the rejected CPU without falling back or advancing the cursor.
    /// Configuration and cursor belong to this queue for its entire lifetime.
    pub(super) fn select_cpu(
        &self,
        target: Option<LogicalCpuId>,
        current_cpu: impl FnOnce() -> LogicalCpuId,
    ) -> Result<LogicalCpuId, LogicalCpuId> {
        let config = &self.config;
        let cpu = match target {
            Some(cpu) => cpu,
            None => match config.policy() {
                CpuPolicy::CurrentCpu => current_cpu(),
                CpuPolicy::RoundRobin => config.cpus()[self.next_cpu_index.load(Ordering::Relaxed)],
            },
        };
        if config.cpus().binary_search(&cpu).is_err() {
            return Err(cpu);
        }
        Ok(cpu)
    }

    /// Advance once after automatic RR acceptance, never on rejection or retry.
    pub(super) fn advance_round_robin(&self) {
        let count = self.config.cpus().len();
        // Bounded wrap avoids usize overflow disturbing non-power-of-two
        // CPU sets. Relaxed suffices: this counter publishes no other data.
        self.next_cpu_index
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                Some(if next == count - 1 { 0 } else { next + 1 })
            })
            .expect("unconditional round-robin advance");
    }
}

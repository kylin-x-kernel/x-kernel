// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Queue-side acquisition and demand observation.

use kcpu_id_map::LogicalCpuId;

use super::Queue;
use crate::work::execution::Execution;

impl Queue {
    /// Claims the CPU's earliest executable Work and samples remaining demand.
    ///
    /// The provider must already have an execution context for this CPU and finish
    /// source inspection before running the returned capability. A missing CPU
    /// or empty ready list returns (None, false); waiting work is not ready demand.
    /// Closure does not revoke previously accepted work. This is not autonomous
    /// scheduling; QueueSource publishes demand and returns independent kicks,
    /// but runtime progress remains provider work. The bool is only a snapshot.
    ///
    /// # Panics
    /// Panics if intrusive membership and the protected Work owner disagree.
    pub(crate) fn claim_with_demand(&self, cpu_id: LogicalCpuId) -> (Option<Execution>, bool) {
        let Some(partition) = self.partition(cpu_id) else {
            return (None, false);
        };
        partition.claim_with_demand()
    }
}

#[cfg(unittest)]
mod tests;

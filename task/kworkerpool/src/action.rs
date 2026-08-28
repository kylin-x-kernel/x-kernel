// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Runtime actions produced by worker-pool state transitions.

use ktime_types::MonotonicInstant;

use crate::{PoolId, WorkerId};

/// Fast-path action that can be executed by the current runtime context after
/// the pool lock is dropped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImmediateAction {
    /// Wake an existing runtime worker.
    WakeWorker { pool: PoolId, worker: WorkerId },
    /// Raise the bottom-half drain context for this pool.
    RaiseBottomHalf { pool: PoolId },
    /// Arm the CPU-intensive accounting deadline for a running worker.
    ArmCpuIntensiveTimer {
        pool: PoolId,
        deadline: MonotonicInstant,
    },
    /// Wake the CPU-local worker-pool manager.
    WakeManager { pool: PoolId },
}

/// Slow-path lifecycle action owned by the per-CPU manager.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagementAction {
    /// Spawn a runtime worker for a reserved slot.
    SpawnWorker { pool: PoolId, worker: WorkerId },
    /// Wake a retiring worker so it can exit in its own context.
    RetireWorker { pool: PoolId, worker: WorkerId },
}

/// Small fixed action batch returned by pool state transitions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActionBatch {
    immediate: [Option<ImmediateAction>; Self::MAX_IMMEDIATE],
}

impl ActionBatch {
    const MAX_IMMEDIATE: usize = 4;

    /// Creates an empty action batch.
    pub const fn new() -> Self {
        Self {
            immediate: [const { None }; Self::MAX_IMMEDIATE],
        }
    }

    pub(crate) fn push_immediate(&mut self, action: ImmediateAction) {
        if let Some(slot) = self.immediate.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some(action);
        }
    }

    /// Appends actions from another batch until this fixed batch is full.
    pub fn append(&mut self, other: Self) {
        for action in other.immediate.into_iter().flatten() {
            self.push_immediate(action);
        }
    }

    /// Iterates over immediate actions.
    pub fn immediate(&self) -> impl Iterator<Item = ImmediateAction> + '_ {
        self.immediate.iter().copied().flatten()
    }

    /// Returns whether the batch has no action.
    pub fn is_empty(&self) -> bool {
        self.immediate().next().is_none()
    }
}

impl Default for ActionBatch {
    fn default() -> Self {
        Self::new()
    }
}

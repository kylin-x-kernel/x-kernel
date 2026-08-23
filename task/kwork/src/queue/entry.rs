// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec::Vec;

use crate::{QueueOwner, ScheduledWork, WorkBarrier, WorkBarrierQueue, WorkColor, WorkInstanceId};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BarrierAttachResult {
    Attached,
    Full,
}

/// Entry stored on a pending work ring.
///
/// Linux stores runnable work on `worker_pool::worklist` and throttled work on
/// `pool_workqueue::inactive_works`. The X-Kernel storage layer mirrors that
/// split with separate fixed-capacity rings, so a [`WorkEntry`] only carries
/// identity, ownership, and pending barriers; runnable vs inactive is expressed
/// by the ring that owns the entry.
pub(crate) struct WorkEntry {
    work: ScheduledWork,
    owner: QueueOwner,
    binding_key: usize,
    color: WorkColor,
    instance_id: WorkInstanceId,
    /// Flush barriers linked after this pending entry.
    ///
    /// This is the X-Kernel storage counterpart of Linux setting
    /// `WORK_STRUCT_LINKED` on a queued work and placing `wq_barrier` directly
    /// after it. The storage is bounded; the entry's runnable/inactive lane
    /// remains separate from barrier ownership.
    barriers: WorkBarrierQueue,
}

impl WorkEntry {
    pub(crate) fn new(
        work: ScheduledWork,
        owner: QueueOwner,
        color: WorkColor,
        instance_id: WorkInstanceId,
    ) -> Self {
        let binding_key = owner.queue().key();
        Self {
            work,
            owner,
            binding_key,
            color,
            instance_id,
            barriers: WorkBarrierQueue::new(),
        }
    }

    pub(crate) fn work(&self) -> &ScheduledWork {
        &self.work
    }

    pub(crate) fn owner(&self) -> &QueueOwner {
        &self.owner
    }

    pub(crate) fn binding_key(&self) -> usize {
        self.binding_key
    }

    pub(crate) fn color(&self) -> WorkColor {
        self.color
    }

    pub(crate) fn instance_id(&self) -> WorkInstanceId {
        self.instance_id
    }

    pub(crate) fn attach_barrier(&mut self, barrier: WorkBarrier) -> BarrierAttachResult {
        if self.barriers.push_front_linked_barrier(barrier) {
            BarrierAttachResult::Attached
        } else {
            BarrierAttachResult::Full
        }
    }

    pub(crate) fn barrier_count(&self) -> usize {
        self.barriers.len()
    }

    pub(crate) fn take_barriers(&mut self) -> Vec<WorkBarrier> {
        self.barriers.take_all()
    }

    pub(crate) fn into_work(self) -> ScheduledWork {
        self.work
    }
}

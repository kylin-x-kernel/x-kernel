// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use super::binding::PoolWake;
use crate::{
    CancelWorkResult, QueueInstanceCompletion, QueueWake, QueueWorkResult, ScheduledWork,
    WorkBarrier, WorkQueuePoolBinding, WorkStatus, WorkerExecutionToken,
};

pub(crate) enum WorkQueuePoolEnqueue {
    Queued(WorkQueuePoolQueued),
    Rejected(QueueWorkResult),
}

pub(crate) struct WorkQueuePoolQueued {
    pub(crate) pool_wake: PoolWake,
    pub(crate) work_state_change: kpoll::PollSet,
}

pub(crate) enum WorkQueuePoolPendingCancel {
    Done(WorkQueuePoolPendingCancelDone),
    Retry,
}

pub(crate) enum WorkQueuePoolBarrierAttach {
    Attached(WorkBarrier),
    Gone,
    Full,
    Retry,
}

pub(crate) struct WorkQueuePoolPendingCancelDone {
    pub(crate) result: CancelWorkResult,
    pub(crate) removed: Option<ScheduledWork>,
    pub(crate) work_done: Option<kpoll::PollSet>,
    pub(crate) work_state_change: Option<kpoll::PollSet>,
    pub(crate) queue_completion: Option<QueueInstanceCompletion>,
    pub(crate) barriers: alloc::vec::Vec<WorkBarrier>,
    pub(crate) pool_wake: Option<PoolWake>,
}

pub(crate) struct WorkQueuePoolRunningFinish {
    pub(crate) queue_completion: QueueInstanceCompletion,
    pub(crate) running_barriers: alloc::vec::Vec<WorkBarrier>,
    pub(crate) pool_wake: Option<PoolWake>,
}

pub(crate) struct WorkQueuePoolRunnableTake {
    pub(crate) work: Option<ScheduledWork>,
    pub(crate) binding: Option<WorkQueuePoolBinding>,
    pub(crate) worker_token: Option<WorkerExecutionToken>,
    pub(crate) completed_barriers: alloc::vec::Vec<WorkBarrier>,
    pub(crate) queue_wakes: alloc::vec::Vec<QueueWake>,
    pub(crate) stale_statuses: alloc::vec::Vec<WorkStatus>,
}

impl WorkQueuePoolPendingCancelDone {
    pub(super) fn new(result: CancelWorkResult) -> Self {
        Self {
            result,
            removed: None,
            work_done: None,
            work_state_change: None,
            queue_completion: None,
            barriers: alloc::vec::Vec::new(),
            pool_wake: None,
        }
    }

    pub(super) fn with_removed(mut self, removed: Option<ScheduledWork>) -> Self {
        self.removed = removed;
        self
    }

    pub(super) fn with_work_done(mut self, waiters: kpoll::PollSet) -> Self {
        self.work_done = Some(waiters);
        self
    }

    pub(super) fn with_queue_completion(mut self, completion: QueueInstanceCompletion) -> Self {
        self.queue_completion = Some(completion);
        self
    }

    pub(super) fn with_barriers(mut self, mut barriers: alloc::vec::Vec<WorkBarrier>) -> Self {
        self.barriers.append(&mut barriers);
        self
    }

    pub(super) fn with_pool_wake(mut self, wake: PoolWake) -> Self {
        self.pool_wake = Some(wake);
        self
    }
}

impl WorkQueuePoolEnqueue {
    pub(super) fn rejected(result: QueueWorkResult) -> Self {
        debug_assert_ne!(result, QueueWorkResult::Queued);
        Self::Rejected(result)
    }

    pub(super) fn queued(work: &ScheduledWork, pool_wake: PoolWake) -> Self {
        Self::Queued(WorkQueuePoolQueued {
            pool_wake,
            work_state_change: work.notify_state_change_defer(),
        })
    }
}

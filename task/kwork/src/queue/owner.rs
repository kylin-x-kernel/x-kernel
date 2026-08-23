// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use super::workqueue::{WorkQueue, WorkQueueHandle};

/// Identity of the logical queue that owns a work instance.
///
/// Linux stores this relationship through `pool_workqueue`. X-Kernel keeps the
/// queue identity explicit so pending/running work can be matched back to both
/// static built-in queues and refcounted dynamic queues without exposing the
/// execution-pool layout to work state.
#[derive(Clone)]
pub(crate) enum QueueOwner {
    Static(&'static WorkQueue),
    Dynamic(WorkQueueHandle),
}

impl QueueOwner {
    pub(crate) fn queue(&self) -> &WorkQueue {
        match self {
            Self::Static(queue) => queue,
            Self::Dynamic(queue) => queue.queue(),
        }
    }

    pub(crate) fn name(&self) -> &'static str {
        self.queue().name()
    }

    pub(crate) fn same_queue(&self, queue: &WorkQueue) -> bool {
        core::ptr::eq(self.queue(), queue)
    }
}

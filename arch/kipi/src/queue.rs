// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::collections::VecDeque;

use kcpu_id_map::LogicalCpuId;

use crate::event::{Callback, IpiEvent};

/// A per-CPU queue of IPI events.
///
/// Uses FIFO ordering (VecDeque) to ensure callbacks are executed
/// in the order they were enqueued.
pub(crate) struct IpiEventQueue {
    events: VecDeque<IpiEvent>,
}

impl IpiEventQueue {
    /// Creates a new empty IPI event queue.
    pub(crate) fn new() -> Self {
        Self {
            events: VecDeque::new(),
        }
    }

    /// Enqueues a new event into this queue.
    pub(crate) fn push(&mut self, src_cpu_id: LogicalCpuId, callback: Callback) {
        self.events.push_back(IpiEvent {
            src_cpu_id,
            callback,
        });
    }

    /// Dequeues the oldest event from this queue.
    ///
    /// Returns `None` if the queue is empty.
    #[must_use]
    pub(crate) fn pop_one(&mut self) -> Option<(LogicalCpuId, Callback)> {
        if let Some(e) = self.events.pop_front() {
            Some((e.src_cpu_id, e.callback))
        } else {
            None
        }
    }
}

impl Default for IpiEventQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[unittest::mod_test]
pub(crate) mod tests_queue {
    include!("../ut-tests/queue_unittest.rs");
}

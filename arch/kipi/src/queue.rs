// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::collections::VecDeque;

use crate::event::{Callback, IpiEvent};

/// A per-CPU queue of IPI events.
///
/// Uses FIFO ordering (VecDeque) to ensure callbacks are executed
/// in the order they were enqueued.
pub struct IpiEventQueue {
    events: VecDeque<IpiEvent>,
}

impl IpiEventQueue {
    /// Creates a new empty IPI event queue.
    pub fn new() -> Self {
        Self {
            events: VecDeque::new(),
        }
    }

    /// Checks if there are no pending events.
    #[allow(dead_code)]
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Enqueues a new event into this queue.
    pub fn push(&mut self, src_cpu_id: usize, callback: Callback) {
        self.events.push_back(IpiEvent {
            src_cpu_id,
            callback,
        });
    }

    /// Dequeues the oldest event from this queue.
    ///
    /// Returns `None` if the queue is empty.
    #[must_use]
    pub fn pop_one(&mut self) -> Option<(usize, Callback)> {
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

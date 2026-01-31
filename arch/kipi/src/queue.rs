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

#[cfg(unittest)]
#[allow(missing_docs)]
pub mod tests_queue {
    use unittest::def_test;

    use super::IpiEventQueue;
    use crate::event::Callback;

    #[def_test]
    fn test_queue_empty_pop() {
        let mut queue = IpiEventQueue::new();
        assert!(queue.pop_one().is_none());
    }

    #[def_test]
    fn test_queue_fifo() {
        let mut queue = IpiEventQueue::new();
        queue.push(1, Callback::new(|| {}));
        queue.push(2, Callback::new(|| {}));
        let (src1, _) = queue.pop_one().unwrap();
        let (src2, _) = queue.pop_one().unwrap();
        assert_eq!(src1, 1);
        assert_eq!(src2, 2);
    }

    #[def_test]
    fn test_queue_reuse() {
        let mut queue = IpiEventQueue::new();
        queue.push(3, Callback::new(|| {}));
        let _ = queue.pop_one();
        queue.push(4, Callback::new(|| {}));
        let (src, _) = queue.pop_one().unwrap();
        assert_eq!(src, 4);
    }
}

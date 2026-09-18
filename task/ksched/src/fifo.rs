// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use super::ready_list::{ReadyItem, ReadyLinks, ReadyList};
use crate::{BaseScheduler, CurrentDisposition};

/// A task wrapper with embedded FIFO ready-queue linkage.
pub struct FifoTask<T> {
    inner: T,
    links: ReadyLinks,
}

impl<T> FifoTask<T> {
    /// Creates a task with detached linkage.
    pub const fn new(inner: T) -> Self {
        Self {
            inner,
            links: ReadyLinks::new(),
        }
    }

    /// Returns the wrapped task.
    pub const fn inner(&self) -> &T {
        &self.inner
    }

    /// Consumes the detached wrapper and returns its task.
    pub fn into_inner(self) -> T {
        self.inner
    }
}

impl<T> core::ops::Deref for FifoTask<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.inner
    }
}

// SAFETY: OFFSET selects this private field, accessed only by ReadyList.
unsafe impl<T> ReadyItem for FifoTask<T> {
    const OFFSET: usize = core::mem::offset_of!(Self, links);

    fn ready_links(&self) -> &ReadyLinks {
        &self.links
    }
}

/// A simple FIFO (First-In-First-Out) cooperative scheduler.
///
/// When a task is added to the scheduler, it's placed at the end of the ready
/// queue. When picking the next task to run, the head of the ready queue is
/// taken.
///
/// As it's a cooperative scheduler, it never requests a schedule timer and
/// never forces preemption from runtime accounting.
///
/// It internally uses a linked list as the ready queue.
pub struct FifoScheduler<T> {
    ready_queue: ReadyList<FifoTask<T>>,
}

impl<T> FifoScheduler<T> {
    /// Creates a new empty [`FifoScheduler`].
    pub const fn new() -> Self {
        Self {
            ready_queue: ReadyList::new(),
        }
    }

    /// get the name of scheduler
    pub fn scheduler_name() -> &'static str {
        "FIFO"
    }
}

impl<T> BaseScheduler for FifoScheduler<T> {
    type SchedItem = Arc<FifoTask<T>>;

    fn init(&mut self) {}

    fn add_task(&mut self, task: Self::SchedItem) {
        self.ready_queue.push_back(task);
    }

    fn remove_task(&mut self, task: &Self::SchedItem) -> Option<Self::SchedItem> {
        self.ready_queue.remove(task)
    }

    fn steal_ready_task<F>(&mut self, want: F) -> Option<Self::SchedItem>
    where
        F: FnMut(&Self::SchedItem) -> bool,
    {
        self.ready_queue.steal(want)
    }

    fn pick_next_task(&mut self) -> Option<Self::SchedItem> {
        self.ready_queue.pop_front()
    }

    fn enqueue_task(&mut self, task: Self::SchedItem) {
        self.ready_queue.push_back(task);
    }

    fn leave_current(&mut self, current: Self::SchedItem, disposition: CurrentDisposition) {
        match disposition {
            CurrentDisposition::Yield | CurrentDisposition::Preempt => {
                self.ready_queue.push_back(current);
            }
            CurrentDisposition::Block | CurrentDisposition::Migrate | CurrentDisposition::Exit => {}
        }
    }

    fn update_current(&mut self, _current: &Self::SchedItem, _elapsed_ns: u64) -> bool {
        false
    }

    fn next_preemption_ns(&self, _current: &Self::SchedItem) -> Option<u64> {
        None
    }

    fn set_priority(&mut self, _task: &Self::SchedItem, _prio: isize) -> bool {
        false
    }
}

impl<T> Default for FifoScheduler<T> {
    fn default() -> Self {
        Self::new()
    }
}

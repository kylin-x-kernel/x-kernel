// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use linked_list_r4l::{List, def_node};

use crate::{BaseScheduler, CurrentDisposition};

def_node! {
    /// A task wrapper for the [`FifoScheduler`].
    ///
    /// It add extra states to use in [`linked_list::List`].
    pub struct FifoTask<T>(T);
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
    ready_queue: List<Arc<FifoTask<T>>>,
}

/// Unlink the first ready waiter matching `want`.
///
/// Idle-pull holds the source scheduler lock on the busiest CPU. Draining the
/// whole list would be O(n) even when the head matches; stop at the hit so the
/// tail stays linked and the cost is O(k) for the match index.
fn steal_first_matching<T>(
    queue: &mut List<Arc<FifoTask<T>>>,
    mut want: impl FnMut(&Arc<FifoTask<T>>) -> bool,
) -> Option<Arc<FifoTask<T>>> {
    let mut skipped = List::new();
    while let Some(task) = queue.pop_front() {
        if want(&task) {
            prepend_in_order(queue, skipped);
            return Some(task);
        }
        skipped.push_back(task);
    }
    *queue = skipped;
    None
}

fn prepend_in_order<T>(dest: &mut List<Arc<FifoTask<T>>>, mut front: List<Arc<FifoTask<T>>>) {
    let mut rev: List<Arc<FifoTask<T>>> = List::new();
    while let Some(task) = front.pop_front() {
        rev.push_front(task);
    }
    while let Some(task) = rev.pop_front() {
        dest.push_front(task);
    }
}

impl<T> FifoScheduler<T> {
    /// Creates a new empty [`FifoScheduler`].
    pub const fn new() -> Self {
        Self {
            ready_queue: List::new(),
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
        // SAFETY: scheduler tasks are only linked into `ready_queue` through
        // this scheduler, so removing a known task preserves the intrusive-list
        // membership invariant.
        unsafe { self.ready_queue.remove(task) }
    }

    fn steal_ready_task<F>(&mut self, want: F) -> Option<Self::SchedItem>
    where
        F: FnMut(&Self::SchedItem) -> bool,
    {
        steal_first_matching(&mut self.ready_queue, want)
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

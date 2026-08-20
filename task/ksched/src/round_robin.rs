// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::{
    ops::Deref,
    sync::atomic::{AtomicU64, Ordering},
};

use linked_list_r4l::{GetLinks, Links, List};

use crate::{BaseScheduler, CurrentDisposition};

/// A task wrapper for the [`RRScheduler`].
///
/// It adds a remaining runtime counter (nanoseconds) for round-robin scheduling.
pub struct RRTask<T, const MAX_SLICE_NS: usize> {
    inner: T,
    remaining_ns: AtomicU64,
    links: Links<Self>,
}

impl<T, const S: usize> RRTask<T, S> {
    /// Creates a new [`RRTask`] from the inner task struct.
    pub const fn new(inner: T) -> Self {
        Self {
            inner,
            remaining_ns: AtomicU64::new(S as u64),
            links: Links::new(),
        }
    }

    fn remaining_ns(&self) -> u64 {
        self.remaining_ns.load(Ordering::Acquire)
    }

    fn reset_slice(&self) {
        self.remaining_ns.store(S as u64, Ordering::Release);
    }

    /// Returns a reference to the inner task struct.
    pub const fn inner(&self) -> &T {
        &self.inner
    }
}

impl<T, const MAX_SLICE_NS: usize> GetLinks for RRTask<T, MAX_SLICE_NS> {
    type EntryType = Self;

    fn get_links(data: &Self::EntryType) -> &Links<Self::EntryType> {
        &data.links
    }
}

impl<T, const S: usize> Deref for RRTask<T, S> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

/// A simple [Round-Robin] (RR) preemptive scheduler.
///
/// Every task has a remaining runtime budget in nanoseconds. When the budget
/// reaches zero, the task is preempted and needs to be rescheduled.
///
/// It internally uses a linked list as the ready queue.
///
/// [Round-Robin]: https://en.wikipedia.org/wiki/Round-robin_scheduling
/// [`FifoScheduler`]: crate::FifoScheduler
pub struct RRScheduler<T, const MAX_SLICE_NS: usize> {
    ready_queue: List<Arc<RRTask<T, MAX_SLICE_NS>>>,
}

/// Unlink the first ready waiter matching `want`.
///
/// Same idle-pull lock concern as [`crate::FifoScheduler`]: do not drain the
/// whole ready list after the first hit.
fn steal_first_matching<T, const S: usize>(
    queue: &mut List<Arc<RRTask<T, S>>>,
    mut want: impl FnMut(&Arc<RRTask<T, S>>) -> bool,
) -> Option<Arc<RRTask<T, S>>> {
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

fn prepend_in_order<T, const S: usize>(
    dest: &mut List<Arc<RRTask<T, S>>>,
    mut front: List<Arc<RRTask<T, S>>>,
) {
    let mut rev: List<Arc<RRTask<T, S>>> = List::new();
    while let Some(task) = front.pop_front() {
        rev.push_front(task);
    }
    while let Some(task) = rev.pop_front() {
        dest.push_front(task);
    }
}

impl<T, const S: usize> RRScheduler<T, S> {
    /// Creates a new empty [`RRScheduler`].
    pub const fn new() -> Self {
        Self {
            ready_queue: List::new(),
        }
    }

    /// get the name of scheduler
    pub fn scheduler_name() -> &'static str {
        "Round-robin"
    }
}

impl<T, const S: usize> BaseScheduler for RRScheduler<T, S> {
    type SchedItem = Arc<RRTask<T, S>>;

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
        task.reset_slice();
        self.ready_queue.push_back(task);
    }

    fn leave_current(&mut self, current: Self::SchedItem, disposition: CurrentDisposition) {
        match disposition {
            CurrentDisposition::Preempt if current.remaining_ns() > 0 => {
                self.ready_queue.push_front(current);
            }
            CurrentDisposition::Yield | CurrentDisposition::Preempt => {
                current.reset_slice();
                self.ready_queue.push_back(current);
            }
            CurrentDisposition::Block | CurrentDisposition::Migrate | CurrentDisposition::Exit => {}
        }
    }

    fn update_current(&mut self, current: &Self::SchedItem, elapsed_ns: u64) -> bool {
        let old = current.remaining_ns.load(Ordering::Acquire);
        let consumed = elapsed_ns.min(old);
        current
            .remaining_ns
            .store(old.saturating_sub(consumed), Ordering::Release);
        old <= elapsed_ns
    }

    fn next_preemption_ns(&self, current: &Self::SchedItem) -> Option<u64> {
        if self.ready_queue.is_empty() {
            return None;
        }
        Some(current.remaining_ns())
    }

    fn set_priority(&mut self, _task: &Self::SchedItem, _prio: isize) -> bool {
        false
    }
}

impl<T, const S: usize> Default for RRScheduler<T, S> {
    fn default() -> Self {
        Self::new()
    }
}

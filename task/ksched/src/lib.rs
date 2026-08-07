// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![cfg_attr(not(test), no_std)]

mod cfs;
mod eevdf;
mod fifo;
mod per_cpu;
mod round_robin;
#[cfg(unittest)]
mod tests;

extern crate alloc;

pub use cfs::{CFSTask, CFScheduler};
pub use eevdf::{EevdfEntity, EevdfScheduler, EevdfStats};
pub use fifo::{FifoScheduler, FifoTask};
pub use per_cpu::{HasSchedulerId, PerCpuScheduler, SchedulerKind};
pub use round_robin::{RRScheduler, RRTask};

/// The base scheduler trait that all schedulers should implement.
///
/// All tasks in the scheduler are considered runnable. If a task is go to
/// sleep, it should be removed from the scheduler.
pub trait BaseScheduler {
    /// Type of scheduled entities. Often a task struct.
    type SchedItem;

    /// Initializes the scheduler.
    fn init(&mut self);

    /// Adds a task to the scheduler.
    fn add_task(&mut self, task: Self::SchedItem);

    /// Removes a task by its reference from the scheduler. Returns the owned
    /// removed task with ownership if it exists.
    ///
    /// # Safety
    ///
    /// The caller should ensure that the task is in the scheduler, otherwise
    /// the behavior is undefined.
    fn remove_task(&mut self, task: &Self::SchedItem) -> Option<Self::SchedItem>;

    /// Picks the next task to run, it will be removed from the scheduler.
    /// Returns [`None`] if there is not runnable task.
    fn pick_next_task(&mut self) -> Option<Self::SchedItem>;

    /// Puts the previous task back to the scheduler. The previous task is
    /// usually placed at the end of the ready queue, making it less likely
    /// to be re-scheduled.
    ///
    /// `preempt` indicates whether the previous task is preempted by the next
    /// task. In this case, the previous task may be placed at the front of the
    /// ready queue.
    fn put_prev_task(&mut self, prev: Self::SchedItem, preempt: bool);

    /// Advances the scheduler state at each timer tick. Returns `true` if
    /// re-scheduling is required.
    ///
    /// `current` is the current running task.
    fn task_tick(&mut self, current: &Self::SchedItem) -> bool;

    /// set priority for a task
    fn set_priority(&mut self, task: &Self::SchedItem, prio: isize) -> bool;

    /// Accounts for a running task that is leaving the CPU without being
    /// requeued (typically sleep / block).
    ///
    /// Fair schedulers such as EEVDF use this to snapshot virtual lag so the
    /// later [`Self::put_prev_task`] wake path can place the task correctly.
    /// The default implementation is a no-op.
    fn account_sleep(&mut self, _task: &Self::SchedItem) {}

    /// Releases any scheduler-owned references to a task that is exiting
    /// without being requeued.
    ///
    /// Schedulers that cache the current task (e.g. EEVDF's `curr`) must drop
    /// that reference here; otherwise an exiting task can be kept alive by the
    /// scheduler after the CPU switched away, which strands its kernel stack
    /// and address space. The default implementation is a no-op.
    fn on_task_exit(&mut self, _task: &Self::SchedItem) {}
}

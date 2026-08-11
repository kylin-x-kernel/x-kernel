// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![cfg_attr(not(test), no_std)]

mod cfs;
mod eevdf;
mod fifo;
mod round_robin;
#[cfg(unittest)]
mod tests;

extern crate alloc;

pub use cfs::{CFSTask, CFScheduler};
pub use eevdf::{EevdfEntity, EevdfScheduler, EevdfStats};
pub use fifo::{FifoScheduler, FifoTask};
pub use round_robin::{RRScheduler, RRTask};

/// How a running task leaves its current execution slot on a run queue.
///
/// This is the only scheduler-visible reason a current task may depart.
/// Callers must use [`BaseScheduler::leave_current`] rather than open-coding
/// sleep accounting or requeue logic per path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CurrentDisposition {
    /// Voluntary yield: reset the request/slice and requeue as Ready.
    Yield,
    /// Involuntary preemption: keep any remaining request and requeue as Ready.
    Preempt,
    /// Block/sleep: leave without requeue; fair schedulers snapshot lag for PLACE_LAG.
    Block,
    /// Migrate away from this RQ: leave without requeue; fair schedulers snapshot
    /// lag so the destination RQ can PLACE_LAG on enqueue.
    Migrate,
    /// Exit: leave without requeue and without arming future placement.
    Exit,
}

/// The base scheduler trait that all schedulers should implement.
///
/// Ready tasks live in the scheduler. A running task is outside the ready
/// queue and must leave through [`Self::leave_current`]. Non-current ready
/// tasks (wake / migrate-in) enter through [`Self::enqueue_task`].
pub trait BaseScheduler {
    /// Type of scheduled entities. Often a task struct.
    type SchedItem;

    /// Initializes the scheduler.
    fn init(&mut self);

    /// Adds a newly created runnable task to the scheduler.
    fn add_task(&mut self, task: Self::SchedItem);

    /// Removes a ready task by reference. Returns the owned task if present.
    ///
    /// # Safety
    ///
    /// The caller should ensure that the task is in the scheduler, otherwise
    /// the behavior is undefined.
    fn remove_task(&mut self, task: &Self::SchedItem) -> Option<Self::SchedItem>;

    /// Picks the next task to run and removes it from the ready queue.
    /// Returns [`None`] if there is no runnable task.
    ///
    /// Requires that any previous current task has already left via
    /// [`Self::leave_current`].
    fn pick_next_task(&mut self) -> Option<Self::SchedItem>;

    /// Enqueues a non-current Ready task (wake or migrate-in).
    ///
    /// Fair schedulers apply saved lag placement (`PLACE_LAG`) when the task
    /// was previously deactivated with [`CurrentDisposition::Block`] or
    /// [`CurrentDisposition::Migrate`].
    fn enqueue_task(&mut self, task: Self::SchedItem);

    /// Transitions the running task out of the current execution slot.
    ///
    /// - [`CurrentDisposition::Yield`] / [`CurrentDisposition::Preempt`]:
    ///   requeue onto this RQ as Ready.
    /// - [`CurrentDisposition::Block`] / [`CurrentDisposition::Migrate`]:
    ///   do not requeue; fair schedulers snapshot lag for a later
    ///   [`Self::enqueue_task`].
    /// - [`CurrentDisposition::Exit`]: do not requeue and do not arm placement.
    fn leave_current(&mut self, current: Self::SchedItem, disposition: CurrentDisposition);

    /// Advances the scheduler state at each timer tick. Returns `true` if
    /// re-scheduling is required.
    ///
    /// `current` is the current running task.
    fn task_tick(&mut self, current: &Self::SchedItem) -> bool;

    /// set priority for a task
    fn set_priority(&mut self, task: &Self::SchedItem, prio: isize) -> bool;
}

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

/// Default EEVDF request length in nanoseconds (2 ms).
///
/// Aligned with Linux `sched_base_slice_ns` on small SMP (roughly 0.7–2 ms).
/// The historical value was 50 ms (`MAX_TIME_SLICE = 5` at 100 Hz); that made
/// same-CPU wake latency under load sit near one full request (~50 ms).
pub const DEFAULT_SLICE_NS: u64 = 2_000_000;

/// Default round-robin quantum in nanoseconds (50 ms).
///
/// RR keeps its historical quantum: EEVDF's latency-oriented request size
/// must not multiply RR's context-switch rate.
pub const DEFAULT_RR_SLICE_NS: u64 = 50_000_000;

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

    /// Removes one ready waiter matching `want`, without selecting it to run.
    ///
    /// Unlike [`Self::pick_next_task`], this does not install a current entity.
    /// Fair schedulers snapshot lag as in [`Self::remove_task`] so a later
    /// [`Self::enqueue_task`] can PLACE_LAG on the destination run queue.
    /// The caller (ktask idle-pull) must reject still-`on_cpu` tasks.
    fn steal_ready_task<F>(&mut self, want: F) -> Option<Self::SchedItem>
    where
        F: FnMut(&Self::SchedItem) -> bool;

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

    /// Accounts `elapsed_ns` of wall-clock runtime for `current`.
    ///
    /// Returns `true` if re-scheduling is required (slice expired or an
    /// eligible peer should preempt).
    fn update_current(&mut self, current: &Self::SchedItem, elapsed_ns: u64) -> bool;

    /// Returns how many nanoseconds from now the scheduler must re-evaluate
    /// preemption for `current`, or [`None`] if no schedule timer is needed
    /// (for example a lone runnable task or a cooperative scheduler).
    fn next_preemption_ns(&self, current: &Self::SchedItem) -> Option<u64>;

    /// set priority for a task
    fn set_priority(&mut self, task: &Self::SchedItem, prio: isize) -> bool;
}

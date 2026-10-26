// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Highest-rank class: the per-CPU stop thread.
//!
//! The caller keeps at most one Stop task on a runqueue. [`ClassOps::enqueue`]
//! asserts when a Stop waiter or a Stop runner is already present.
//! [`ClassOps::set_curr`] asserts when a waiter is already present;
//! [`crate::Scheduler::install_current`] has already rejected a live runner.
//! The failure is a kernel bug: this trait has no error return, and a second
//! Stop task is not a recoverable user failure.
//! The runner is never preempted by a lower class and is not charged a slice.
//! `ktask` does not activate this class yet; when it does, it must create and
//! wake only one Stop task per runqueue.

use crate::{
    class::{ClassOps, EnqueueKind, LeaveReason},
    clock::RqClock,
    entity::{QueueSeat, RqLocked, SchedTask},
    owner::SharedOwner,
    policy::{SchedAttr, SchedClass, SchedError},
};

/// Single per-RQ Stop waiter. The runner stays in `Scheduler::curr`, not here.
pub(crate) struct StopClass<T, Owner: SharedOwner<SchedTask<T>>> {
    waiter: Option<Owner>,
    marker: core::marker::PhantomData<T>,
}

impl<T, Owner: SharedOwner<SchedTask<T>>> StopClass<T, Owner> {
    pub(crate) const fn new() -> Self {
        Self {
            waiter: None,
            marker: core::marker::PhantomData,
        }
    }

    /// Place `task` in the only waiter slot.
    ///
    /// [`ClassOps::enqueue`] checks that the slot is free first. [`ClassOps::put_prev`]
    /// calls this after the runner has left `curr`, when the slot is already empty.
    fn push_waiter(&mut self, task: Owner, tok: RqLocked) {
        task.set_seat(QueueSeat::Stop, tok);
        self.waiter = Some(task);
    }
}

impl<T, Owner: SharedOwner<SchedTask<T>>> ClassOps<T, Owner> for StopClass<T, Owner> {
    fn class(&self) -> SchedClass {
        SchedClass::Stop
    }

    fn nr_queued(&self) -> usize {
        usize::from(self.waiter.is_some())
    }

    fn has_runnable(&self) -> bool {
        self.waiter.is_some()
    }

    fn advance(&mut self, _clk: &RqClock, _tok: RqLocked) {}

    fn enqueue(
        &mut self,
        task: Owner,
        _kind: EnqueueKind,
        curr: Option<&SchedTask<T>>,
        _clk: &RqClock,
        tok: RqLocked,
    ) {
        // The running Stop task occupies the only slot even though it is not
        // stored in `waiter`. A second helper on this RQ would alias it.
        // `put_prev` does not call this: the runner has already left `curr`.
        assert!(
            curr.is_none() && self.waiter.is_none(),
            "at most one Stop task per runqueue"
        );
        self.push_waiter(task, tok);
    }

    fn dequeue(
        &mut self,
        task: &Owner,
        _curr: Option<&SchedTask<T>>,
        _clk: &RqClock,
        tok: RqLocked,
    ) -> Option<Owner> {
        let queued = self.waiter.as_ref()?;
        if !Owner::ptr_eq(queued, task) {
            return None;
        }
        let removed = self.waiter.take()?;
        removed.set_seat(QueueSeat::Detached, tok);
        Some(removed)
    }

    fn steal(
        &mut self,
        _want: &mut dyn FnMut(&SchedTask<T>) -> bool,
        _curr: Option<&SchedTask<T>>,
        _clk: &RqClock,
        _tok: RqLocked,
    ) -> Option<Owner> {
        None
    }

    fn pick_next(&mut self, _clk: &RqClock, tok: RqLocked) -> Option<Owner> {
        let task = self.waiter.take()?;
        task.set_seat(QueueSeat::Detached, tok);
        Some(task)
    }

    fn set_curr(&mut self, _task: &SchedTask<T>, _clk: &RqClock, _tok: RqLocked) {
        // `install_current` never calls `enqueue`. A waiter already occupies
        // the only Stop task this runqueue may have.
        assert!(self.waiter.is_none(), "at most one Stop task per runqueue");
    }

    fn put_prev(
        &mut self,
        prev: Owner,
        reason: LeaveReason,
        _clk: &RqClock,
        tok: RqLocked,
    ) -> bool {
        match reason {
            LeaveReason::Yield | LeaveReason::Preempt => {
                self.push_waiter(prev, tok);
                true
            }
            // Block, migrate, and exit leave the slot. Stop is pinned to the
            // RQ it entered; a later PR enqueues on the destination, if any.
            LeaveReason::Block | LeaveReason::Migrate | LeaveReason::Exit => false,
        }
    }

    fn account(
        &mut self,
        _curr: &SchedTask<T>,
        _elapsed_ns: u64,
        _clk: &RqClock,
        _tok: RqLocked,
    ) -> bool {
        false
    }

    fn tick_preempts(&mut self, _curr: &SchedTask<T>, _clk: &RqClock, _tok: RqLocked) -> bool {
        false
    }

    fn wakeup_preempts(&mut self, _curr: &SchedTask<T>, _clk: &RqClock, _tok: RqLocked) -> bool {
        false
    }

    fn curr_timer_ns(&self, _curr: &SchedTask<T>, _clk: &RqClock, _tok: RqLocked) -> Option<u64> {
        None
    }

    fn class_timer_ns(&self, _clk: &RqClock, _tok: RqLocked) -> Option<u64> {
        None
    }

    fn attr_changed(
        &mut self,
        _task: &Owner,
        _old: SchedAttr,
        _is_curr: bool,
        _clk: &RqClock,
        _tok: RqLocked,
    ) {
    }

    fn can_admit(
        &self,
        _task: &SchedTask<T>,
        _new: SchedAttr,
        _tok: RqLocked,
    ) -> Result<(), SchedError> {
        Err(SchedError::KernelClassImmutable)
    }

    fn switched_from(
        &mut self,
        _task: &SchedTask<T>,
        _is_curr: bool,
        _clk: &RqClock,
        _tok: RqLocked,
    ) {
    }

    fn switched_to(
        &mut self,
        _task: &SchedTask<T>,
        _is_curr: bool,
        _clk: &RqClock,
        _tok: RqLocked,
    ) {
    }
}

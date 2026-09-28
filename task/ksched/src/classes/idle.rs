// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Lowest-rank class: the per-CPU idle thread.
//!
//! Idle never joins a ready tree. `pick_next` always returns the pinned idle
//! task so the arbitrator never yields `None`.

use alloc::sync::Arc;

use crate::{
    class::{ClassOps, EnqueueKind, LeaveReason},
    clock::RqClock,
    entity::{RqLocked, SchedTask},
    policy::{SchedAttr, SchedClass, SchedError},
};

/// One idle task per RQ. `has_runnable` is always true so pick can fall back.
pub(crate) struct IdleClass<T> {
    idle: Arc<SchedTask<T>>,
}

impl<T> IdleClass<T> {
    pub fn new(idle: Arc<SchedTask<T>>) -> Self {
        Self { idle }
    }

    #[cfg(unittest)]
    pub(crate) fn idle(&self) -> &Arc<SchedTask<T>> {
        &self.idle
    }
}

impl<T> ClassOps<T> for IdleClass<T> {
    fn class(&self) -> SchedClass {
        SchedClass::Idle
    }

    fn nr_queued(&self) -> usize {
        0
    }

    fn has_runnable(&self) -> bool {
        true
    }

    fn advance(&mut self, _clk: &RqClock, _tok: RqLocked) {}

    fn enqueue(
        &mut self,
        _task: Arc<SchedTask<T>>,
        _kind: EnqueueKind,
        _curr: Option<&SchedTask<T>>,
        _clk: &RqClock,
        _tok: RqLocked,
    ) {
        panic!("Idle tasks are never enqueued");
    }

    fn dequeue(
        &mut self,
        _task: &SchedTask<T>,
        _curr: Option<&SchedTask<T>>,
        _clk: &RqClock,
        _tok: RqLocked,
    ) -> Option<Arc<SchedTask<T>>> {
        None
    }

    fn steal(
        &mut self,
        _want: &mut dyn FnMut(&SchedTask<T>) -> bool,
        _curr: Option<&SchedTask<T>>,
        _clk: &RqClock,
        _tok: RqLocked,
    ) -> Option<Arc<SchedTask<T>>> {
        None
    }

    fn pick_next(&mut self, _clk: &RqClock, _tok: RqLocked) -> Option<Arc<SchedTask<T>>> {
        Some(self.idle.clone())
    }

    fn set_curr(&mut self, _task: &SchedTask<T>, _clk: &RqClock, _tok: RqLocked) {}

    fn put_prev(
        &mut self,
        _prev: Arc<SchedTask<T>>,
        _reason: LeaveReason,
        _clk: &RqClock,
        _tok: RqLocked,
    ) -> bool {
        false
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
        _task: &Arc<SchedTask<T>>,
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
        // Reaching Idle from a user class is a kernel bug at the attr boundary.
    }
}

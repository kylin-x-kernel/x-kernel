// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-class operations. The class arbitrator iterates [`ClassOps`] by rank.

use crate::{
    clock::RqClock,
    entity::{RqLocked, SchedTask},
    owner::SharedOwner,
    policy::{SchedAttr, SchedClass, SchedError},
};

/// Why a task is becoming runnable on this RQ.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnqueueKind {
    /// First time runnable (spawn).
    New,
    /// Blocked task waking. `sync` means the waker expects to sleep soon.
    Wake { sync: bool },
    /// Cross-RQ migrate-in or idle-pull landing.
    MigrateIn,
    /// `set_attr` reseat after a class change.
    ClassChange,
}

/// Why the runner is leaving the CPU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeaveReason {
    /// Voluntary yield.
    Yield,
    /// Involuntary preemption.
    Preempt,
    /// Going to sleep.
    Block,
    /// Leaving this CPU for another run queue. Same lag snapshot as
    /// [`Self::Block`]; the destination enqueues with [`EnqueueKind::MigrateIn`].
    Migrate,
    /// Exiting.
    Exit,
}

/// Policy decision after a mutation: whether to reschedule and when to look again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Decision {
    /// A leave/pick should happen at the next safe point.
    pub resched: bool,
    /// Relative nanoseconds until the next check. `None` disarms the timer;
    /// `Some(0)` means check immediately.
    pub next_event_ns: Option<u64>,
}

/// How the class arbitrator `update` path should probe after charging.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateKind {
    /// Charge only; resched if this class exhausted its request.
    Account,
    /// Charge plus same-class tick preemption.
    SchedTick,
    /// Charge plus same-class wakeup preemption.
    WakeProbe,
}

/// Object-safe class operations. Every method runs under the RQ lock.
///
/// Methods that take `curr: Option<&SchedTask<T>>` receive `Some` only when
/// the runner belongs to this class. Methods that take `curr: &SchedTask<T>`
/// are only invoked in that case.
pub(crate) trait ClassOps<T, Owner: SharedOwner<SchedTask<T>>> {
    /// Rank this implementation occupies in the arbitrator.
    fn class(&self) -> SchedClass;

    /// Ready waiters on this class, excluding the runner.
    fn nr_queued(&self) -> usize;

    /// Whether [`Self::pick_next`] can return a task right now.
    ///
    /// Idle reports `true` even with an empty queue so the arbitrator can
    /// always fall back.
    fn has_runnable(&self) -> bool;

    /// Move class-owned time up to `clk`.
    fn advance(&mut self, clk: &RqClock, tok: RqLocked);

    /// Place `task` onto this class's ready structure.
    fn enqueue(
        &mut self,
        task: Owner,
        kind: EnqueueKind,
        curr: Option<&SchedTask<T>>,
        clk: &RqClock,
        tok: RqLocked,
    );

    /// Remove a non-current waiter. `None` if `task` is not queued here.
    fn dequeue(
        &mut self,
        task: &Owner,
        curr: Option<&SchedTask<T>>,
        clk: &RqClock,
        tok: RqLocked,
    ) -> Option<Owner>;

    /// Idle-pull candidate. Idle returns `None`.
    fn steal(
        &mut self,
        want: &mut dyn FnMut(&SchedTask<T>) -> bool,
        curr: Option<&SchedTask<T>>,
        clk: &RqClock,
        tok: RqLocked,
    ) -> Option<Owner>;

    /// Take the next runner from this class, or `None` to try the next rank.
    ///
    /// Idle always returns its pinned task. The arbitrator never sees `None`
    /// after the Idle class.
    fn pick_next(&mut self, clk: &RqClock, tok: RqLocked) -> Option<Owner>;

    /// Boot / class-switch hook for a task that is already the runner.
    ///
    /// Must not enqueue: the arbitrator `curr` slot already owns the `Arc`.
    fn set_curr(&mut self, task: &SchedTask<T>, clk: &RqClock, tok: RqLocked);

    /// The runner is leaving. Requeue on yield/preempt; drop on block/migrate/exit.
    fn put_prev(&mut self, prev: Owner, reason: LeaveReason, clk: &RqClock, tok: RqLocked) -> bool;

    /// Charge `elapsed_ns` to the runner.
    ///
    /// `true` means this class exhausted its request and wants a reschedule.
    fn account(
        &mut self,
        curr: &SchedTask<T>,
        elapsed_ns: u64,
        clk: &RqClock,
        tok: RqLocked,
    ) -> bool;

    /// Same-class tick probe: should a waiting peer preempt `curr`?
    fn tick_preempts(&mut self, curr: &SchedTask<T>, clk: &RqClock, tok: RqLocked) -> bool;

    /// Same-class wake probe.
    fn wakeup_preempts(&mut self, curr: &SchedTask<T>, clk: &RqClock, tok: RqLocked) -> bool;

    /// Relative nanoseconds until this runner needs another look.
    fn curr_timer_ns(&self, curr: &SchedTask<T>, clk: &RqClock, tok: RqLocked) -> Option<u64>;

    /// Relative nanoseconds until a class-wide event.
    fn class_timer_ns(&self, clk: &RqClock, tok: RqLocked) -> Option<u64>;

    /// Replay queued keys after `set_attr` stayed in this class.
    fn attr_changed(
        &mut self,
        task: &Owner,
        old: SchedAttr,
        is_curr: bool,
        clk: &RqClock,
        tok: RqLocked,
    );

    /// Admission check before `set_attr` mutates.
    fn can_admit(
        &self,
        task: &SchedTask<T>,
        new: SchedAttr,
        tok: RqLocked,
    ) -> Result<(), SchedError>;

    /// Leave this class during `set_attr`.
    fn switched_from(&mut self, task: &SchedTask<T>, is_curr: bool, clk: &RqClock, tok: RqLocked);

    /// Enter this class during `set_attr`.
    fn switched_to(&mut self, task: &SchedTask<T>, is_curr: bool, clk: &RqClock, tok: RqLocked);
}

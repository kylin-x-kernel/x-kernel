// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Class arbitrator. `ktask` drives this type on the production path.
//!
//! [`Scheduler`] owns the five class ranks and the runner `Arc`. Stop and
//! Deadline are rank slots: `pick_next` returns [`None`] and `enqueue`
//! panics. RT is fixed-priority FIFO/RR. Fair is EEVDF. Idle is the lowest
//! rank, so a finished pick never returns [`None`]. Every public method takes
//! an absolute
//! `now_ns`, advances [`RqClock`], and returns a [`Decision`] when the
//! caller may need to reschedule or reprogram a timer. This crate does not
//! sample the clock, hold the RQ lock, or switch stacks.

use alloc::sync::Arc;
use core::marker::PhantomData;

use crate::{
    class::{ClassOps, Decision, EnqueueKind, LeaveReason, UpdateKind},
    classes::{DeadlineClass, FairClass, IdleClass, RtClass},
    clock::RqClock,
    entity::{QueueSeat, RqLocked, SchedTask},
    policy::{SchedAttr, SchedClass, SchedError},
};

/// Rank that is defined but has no queue yet.
struct UnwiredClass<T> {
    class: SchedClass,
    _task: PhantomData<T>,
}

impl<T> UnwiredClass<T> {
    const fn new(class: SchedClass) -> Self {
        Self {
            class,
            _task: PhantomData,
        }
    }
}

impl<T> ClassOps<T> for UnwiredClass<T> {
    fn class(&self) -> SchedClass {
        self.class
    }

    fn nr_queued(&self) -> usize {
        0
    }

    fn has_runnable(&self) -> bool {
        false
    }

    fn advance(&mut self, _clk: &RqClock, _tok: RqLocked) {}

    #[expect(
        clippy::panic,
        reason = "CG-021-no-panic-on-reachable-failure: SchedAttr::validate admits only Fair \
                  policies until other classes are wired, so no task reaches an unwired class's \
                  enqueue"
    )]
    fn enqueue(
        &mut self,
        _task: Arc<SchedTask<T>>,
        _kind: EnqueueKind,
        _curr: Option<&SchedTask<T>>,
        _clk: &RqClock,
        _tok: RqLocked,
    ) {
        panic!("enqueue on an unwired scheduling class");
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
        None
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
        Err(SchedError::InvalidPolicy)
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

/// Stop, Deadline, RT, Fair, then Idle. `curr` and `clock` stay outside so a
/// queue can be borrowed mutably beside the runner.
pub(crate) struct ClassSet<T> {
    stop: UnwiredClass<T>,
    deadline: DeadlineClass<T>,
    rt: RtClass<T>,
    fair: FairClass<T>,
    idle: IdleClass<T>,
}

impl<T> ClassSet<T> {
    fn get(&self, c: SchedClass) -> &dyn ClassOps<T> {
        match c {
            SchedClass::Stop => &self.stop,
            SchedClass::Deadline => &self.deadline,
            SchedClass::Rt => &self.rt,
            SchedClass::Fair => &self.fair,
            SchedClass::Idle => &self.idle,
        }
    }

    fn get_mut(&mut self, c: SchedClass) -> &mut dyn ClassOps<T> {
        match c {
            SchedClass::Stop => &mut self.stop,
            SchedClass::Deadline => &mut self.deadline,
            SchedClass::Rt => &mut self.rt,
            SchedClass::Fair => &mut self.fair,
            SchedClass::Idle => &mut self.idle,
        }
    }

    fn iter_by_rank(&self) -> [&dyn ClassOps<T>; 5] {
        [&self.stop, &self.deadline, &self.rt, &self.fair, &self.idle]
    }

    fn iter_by_rank_mut(&mut self) -> [&mut dyn ClassOps<T>; 5] {
        [
            &mut self.stop,
            &mut self.deadline,
            &mut self.rt,
            &mut self.fair,
            &mut self.idle,
        ]
    }
}

/// Per-CPU class arbitrator.
pub struct Scheduler<T> {
    clock: RqClock,
    curr: Option<Arc<SchedTask<T>>>,
    classes: ClassSet<T>,
}

impl<T> Scheduler<T> {
    /// Build an empty scheduler whose Idle fallback is `idle`.
    ///
    /// Fair requests use [`crate::DEFAULT_SLICE_NS`].
    pub fn new(idle: Arc<SchedTask<T>>, now_ns: u64) -> Self {
        Self {
            clock: RqClock::new(now_ns),
            curr: None,
            classes: ClassSet {
                stop: UnwiredClass::new(SchedClass::Stop),
                deadline: DeadlineClass::new(),
                rt: RtClass::new(),
                fair: FairClass::new(),
                idle: IdleClass::new(idle),
            },
        }
    }

    /// RQ-lock token for `RqCell` access.
    ///
    /// `ktask` calls every public method while holding this RQ's lock, so
    /// `RqCell` fields may be read and written.
    fn token() -> RqLocked {
        // SAFETY: ktask serializes all Scheduler calls on this RQ.
        unsafe { RqLocked::assume() }
    }

    /// Runner if it belongs to `class`. Takes the field so callers can still
    /// mutably borrow `classes`.
    fn curr_for(curr: &Option<Arc<SchedTask<T>>>, class: SchedClass) -> Option<&SchedTask<T>> {
        curr.as_deref().filter(|task| task.sched_class() == class)
    }

    /// Notify every class with the post-`advance` clock snapshot.
    ///
    /// `clock` must be a `Copy` of `self.clock` taken after
    /// `RqClock::advance`. Class `advance` and a later `account` /
    /// `pick_next` must not see a different `now_ns`/`last_ns`.
    fn advance_classes(&mut self, clock: &RqClock, tok: RqLocked) {
        for class in self.classes.iter_by_rank_mut() {
            class.advance(clock, tok);
        }
    }

    /// Advance the clock once, notify classes, then charge the runner.
    ///
    /// `elapsed` and `clock` both come from that single `advance`;
    /// `advance_classes` is given the same `Copy` snapshot `account` uses.
    fn charge(&mut self, now_ns: u64, tok: RqLocked) -> bool {
        let elapsed = self.clock.advance(now_ns);
        let clock = self.clock;
        self.advance_classes(&clock, tok);
        let Some(curr) = self.curr.as_deref() else {
            return false;
        };
        let class = curr.sched_class();
        self.classes
            .get_mut(class)
            .account(curr, elapsed, &clock, tok)
    }

    fn higher_class_runnable(&self, than: SchedClass) -> bool {
        self.classes
            .iter_by_rank()
            .into_iter()
            .take_while(|c| c.class().is_above(than))
            .any(|c| c.has_runnable())
    }

    fn resched_needed(&mut self, kind: UpdateKind, exhausted: bool, tok: RqLocked) -> bool {
        let Some(curr) = self.curr.as_deref() else {
            return true;
        };
        let class = curr.sched_class();
        if exhausted || self.higher_class_runnable(class) {
            return true;
        }
        let clock = self.clock;
        match kind {
            UpdateKind::Account => false,
            UpdateKind::SchedTick => self.classes.get_mut(class).tick_preempts(curr, &clock, tok),
            UpdateKind::WakeProbe => self
                .classes
                .get_mut(class)
                .wakeup_preempts(curr, &clock, tok),
        }
    }

    fn compute_next_event(&self, tok: RqLocked) -> Option<u64> {
        let mut next: Option<u64> = None;
        let min_with = |acc: &mut Option<u64>, v: Option<u64>| {
            if let Some(ns) = v {
                *acc = Some(acc.map_or(ns, |a| a.min(ns)));
            }
        };
        if let Some(curr) = &self.curr {
            let cc = curr.sched_class();
            if self.higher_class_runnable(cc) {
                return Some(0);
            }
            min_with(
                &mut next,
                self.classes.get(cc).curr_timer_ns(curr, &self.clock, tok),
            );
        }
        for class in self.classes.iter_by_rank() {
            min_with(&mut next, class.class_timer_ns(&self.clock, tok));
        }
        next
    }

    fn leave(&mut self, now_ns: u64, reason: LeaveReason, tok: RqLocked) -> Arc<SchedTask<T>> {
        let _ = self.charge(now_ns, tok);
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: leave is only entered for the \
                      installed current runner under the runqueue lock"
        )]
        let curr = self.curr.take().expect("leave without runner");
        let class = curr.sched_class();
        // After `charge`, `last_ns == now_ns`. `put_prev` may read this
        // snapshot; it must not derive another elapsed interval from it.
        let clock = self.clock;
        self.classes
            .get_mut(class)
            .put_prev(curr.clone(), reason, &clock, tok);
        curr
    }

    /// Boot: install `task` as the first runner.
    ///
    /// Call this only after [`Scheduler::new`] and before any
    /// [`Self::pick_next`]. The boot `main` (or AP idle) does not go through
    /// the ready queue. Later switches use a leave method, then `pick_next`.
    ///
    /// # Panics
    ///
    /// Panics if a runner is already recorded in `curr`
    /// (`assert!(self.curr.is_none())`).
    ///
    /// # Examples
    ///
    /// `Scheduler::new` leaves `curr` empty; `install_current` records the
    /// first runner. A second call would panic.
    ///
    /// ```rust,ignore
    /// use alloc::sync::Arc;
    /// use ksched::{KernelClass, SchedTask, Scheduler};
    ///
    /// let idle = Arc::new(SchedTask::new_kernel(0usize, KernelClass::Idle));
    /// let mut rq = Scheduler::new(idle, 0);
    /// assert!(rq.current().is_none());
    ///
    ///     let main = Arc::new(SchedTask::new(1usize));
    /// rq.install_current(main.clone(), 0);
    /// assert!(rq.current().is_some_and(|c| Arc::ptr_eq(c, &main)));
    /// ```
    ///
    /// Boot `main` and AP idle call this before the first `pick_next`.
    pub fn install_current(&mut self, task: Arc<SchedTask<T>>, now_ns: u64) {
        let tok = Self::token();
        assert!(self.curr.is_none(), "install_current on a live runner");
        let _ = self.clock.advance(now_ns);
        let clock = self.clock;
        self.advance_classes(&clock, tok);
        let class = task.sched_class();
        self.classes.get_mut(class).set_curr(&task, &clock, tok);
        self.curr = Some(task);
    }

    /// Current runner, if any.
    pub fn current(&self) -> Option<&Arc<SchedTask<T>>> {
        self.curr.as_ref()
    }

    /// Derived: sum of class queues under the RQ lock (idle and the runner
    /// are excluded).
    pub fn nr_queued(&self) -> usize {
        self.classes
            .iter_by_rank()
            .into_iter()
            .map(|c| c.nr_queued())
            .sum()
    }

    /// Charge and probe without switching.
    pub fn update(&mut self, now_ns: u64, kind: UpdateKind) -> Decision {
        let tok = Self::token();
        let exhausted = self.charge(now_ns, tok);
        let resched = self.resched_needed(kind, exhausted, tok);
        Decision {
            resched,
            next_event_ns: self.compute_next_event(tok),
        }
    }

    /// Charge, enqueue, then probe wakeup preemption.
    pub fn enqueue(&mut self, task: Arc<SchedTask<T>>, kind: EnqueueKind, now_ns: u64) -> Decision {
        let tok = Self::token();
        let _ = self.charge(now_ns, tok);
        let class = task.sched_class();
        let clock = self.clock;
        let curr = Self::curr_for(&self.curr, class);
        self.classes
            .get_mut(class)
            .enqueue(task, kind, curr, &clock, tok);
        let resched = self.resched_needed(UpdateKind::WakeProbe, false, tok);
        Decision {
            resched,
            next_event_ns: self.compute_next_event(tok),
        }
    }

    /// Take a queued (non-current) task off this RQ.
    pub fn dequeue(&mut self, task: &SchedTask<T>, now_ns: u64) -> Option<Arc<SchedTask<T>>> {
        let tok = Self::token();
        let _ = self.charge(now_ns, tok);
        if task.seat(tok) == QueueSeat::Detached {
            return None;
        }
        let class = task.sched_class();
        let clock = self.clock;
        let curr = Self::curr_for(&self.curr, class);
        self.classes.get_mut(class).dequeue(task, curr, &clock, tok)
    }

    /// Idle-pull source: steal one RT waiter, then one Fair waiter, that matches `want`.
    pub fn steal(
        &mut self,
        want: &mut dyn FnMut(&SchedTask<T>) -> bool,
        now_ns: u64,
    ) -> Option<Arc<SchedTask<T>>> {
        let tok = Self::token();
        let _ = self.charge(now_ns, tok);
        let clock = self.clock;
        let rt_curr = Self::curr_for(&self.curr, SchedClass::Rt);
        if let Some(task) = self.classes.rt.steal(want, rt_curr, &clock, tok) {
            return Some(task);
        }
        let fair_curr = Self::curr_for(&self.curr, SchedClass::Fair);
        self.classes.fair.steal(want, fair_curr, &clock, tok)
    }

    /// Pick the next runner. Idle always supplies a task.
    ///
    /// # Panics
    ///
    /// A previous runner is still recorded in `curr`.
    pub fn pick_next(&mut self, now_ns: u64) -> Arc<SchedTask<T>> {
        let tok = Self::token();
        assert!(
            self.curr.is_none(),
            "pick_next requires the current task to have already left"
        );
        let _ = self.clock.advance(now_ns);
        let clock = self.clock;
        self.advance_classes(&clock, tok);
        for class in self.classes.iter_by_rank_mut() {
            if let Some(task) = class.pick_next(&clock, tok) {
                self.curr = Some(task.clone());
                return task;
            }
        }
        unreachable!("IdleClass::pick_next always returns a task");
    }

    /// Voluntary yield: requeue the runner.
    pub fn yield_current(&mut self, now_ns: u64) {
        let tok = Self::token();
        let _ = self.leave(now_ns, LeaveReason::Yield, tok);
    }

    /// Involuntary preemption: requeue the runner.
    pub fn preempt_current(&mut self, now_ns: u64) {
        let tok = Self::token();
        let _ = self.leave(now_ns, LeaveReason::Preempt, tok);
    }

    /// The runner is going to sleep. Caller must hold its own `Arc`.
    pub fn block_current(&mut self, now_ns: u64) {
        let tok = Self::token();
        let _ = self.leave(now_ns, LeaveReason::Block, tok);
    }

    /// The runner is moving to another CPU. Caller must hold its own `Arc`
    /// and enqueue it on the destination with [`EnqueueKind::MigrateIn`].
    pub fn migrate_current(&mut self, now_ns: u64) {
        let tok = Self::token();
        let _ = self.leave(now_ns, LeaveReason::Migrate, tok);
    }

    /// The runner is exiting. Returns the `Arc` for the exited list.
    pub fn exit_current(&mut self, now_ns: u64) -> Arc<SchedTask<T>> {
        let tok = Self::token();
        self.leave(now_ns, LeaveReason::Exit, tok)
    }

    /// Unique policy-modification entry. Failure charges but does not mutate.
    ///
    /// # Errors
    ///
    /// Returns the first [`SchedAttr::validate`] failure, including
    /// [`SchedError::InvalidPriority`] for a bad FIFO/RR priority.
    ///
    /// Returns [`SchedError::KernelClassImmutable`] when `task` is Idle.
    ///
    /// # Panics
    ///
    /// Panics if a class change finds `task` queued (`seat != Detached`) but
    /// the old class tree does not hold it.
    pub fn set_attr(
        &mut self,
        task: &Arc<SchedTask<T>>,
        attr: SchedAttr,
        now_ns: u64,
    ) -> Result<Decision, SchedError> {
        attr.validate()?;
        let tok = Self::token();
        if task.sched_class().is_kernel_only() {
            let _ = self.charge(now_ns, tok);
            return Err(SchedError::KernelClassImmutable);
        }
        let _ = self.charge(now_ns, tok);
        let old = task.attr(tok);
        let is_curr = self
            .curr
            .as_ref()
            .is_some_and(|curr| Arc::ptr_eq(curr, task));
        let new_class = attr.class();
        let clock = self.clock;
        self.classes.get(new_class).can_admit(task, attr, tok)?;
        if old.class() == new_class {
            task.set_attr(attr, tok);
            self.classes
                .get_mut(new_class)
                .attr_changed(task, old, is_curr, &clock, tok);
        } else {
            let was_queued = task.seat(tok) != QueueSeat::Detached;
            if !is_curr && was_queued {
                let curr = Self::curr_for(&self.curr, old.class());
                assert!(
                    self.classes
                        .get_mut(old.class())
                        .dequeue(task, curr, &clock, tok)
                        .is_some(),
                    "queued task missing from its class tree"
                );
            }
            self.classes
                .get_mut(old.class())
                .switched_from(task, is_curr, &clock, tok);
            task.set_attr(attr, tok);
            task.set_class(new_class, tok);
            self.classes
                .get_mut(new_class)
                .switched_to(task, is_curr, &clock, tok);
            if !is_curr && was_queued {
                let curr = Self::curr_for(&self.curr, new_class);
                self.classes.get_mut(new_class).enqueue(
                    task.clone(),
                    EnqueueKind::ClassChange,
                    curr,
                    &clock,
                    tok,
                );
            }
        }
        let resched = self.resched_needed(UpdateKind::WakeProbe, false, tok);
        Ok(Decision {
            resched,
            next_event_ns: self.compute_next_event(tok),
        })
    }

    /// Read the task's stored attributes under the RQ lock.
    pub fn attr(&self, task: &SchedTask<T>) -> SchedAttr {
        task.attr(Self::token())
    }

    /// Read-only next-event query after a switch, under the RQ lock.
    pub fn next_event_ns(&self) -> Option<u64> {
        self.compute_next_event(Self::token())
    }
}

#[cfg(unittest)]
impl<T> Scheduler<T> {
    pub(crate) fn curr_is_none(&self) -> bool {
        self.curr.is_none()
    }

    pub(crate) fn fair_mut(&mut self) -> &mut FairClass<T> {
        &mut self.classes.fair
    }

    pub(crate) fn idle_task(&self) -> &Arc<SchedTask<T>> {
        self.classes.idle.idle()
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Deadline class: CBS budget enforcement with EDF ordering.
//!
//! A task reserves `runtime_ns` per `period_ns`. While it has budget left it
//! is ready and ordered by absolute deadline (`deadline_ns`), so the earliest
//! deadline runs first. When the budget is spent the task is throttled and
//! moved to the period-start tree, where it waits until the next period
//! replenishes its budget.
//!
//! Admission is per run queue: the sum of every resident task's
//! `runtime/period` must stay within `1 - margin` of one CPU.

use core::ptr::NonNull;

use intrusive_collections::{Adapter, DefaultLinkOps, KeyAdapter, RBTree, RBTreeAtomicLink};

use crate::{
    class::{ClassOps, EnqueueKind, LeaveReason},
    clock::RqClock,
    config::DL_BW_MARGIN_PPM,
    entity::{QueueSeat, RqLocked, SchedTask},
    owner::{OwnerPointerOps, SharedOwner},
    policy::{DlParams, SchedAttr, SchedClass, SchedError},
};

/// Tree key: `(instant_ns, tree_id)`. Equal instants keep FIFO order.
type DlKey = (u64, u64);

/// Projects [`SchedTask`]'s deadline links for the intrusive trees.
///
/// `READY` selects `deadline.ready_link`; otherwise `deadline.throttled_link`.
/// One const-generic adapter serves both trees, mirroring `FairAdapter`.
struct DlAdapter<T, Owner: SharedOwner<SchedTask<T>>, const READY: bool> {
    link_ops: <RBTreeAtomicLink as DefaultLinkOps>::Ops,
    pointer_ops: OwnerPointerOps<SchedTask<T>, Owner>,
}

impl<T, Owner: SharedOwner<SchedTask<T>>, const R: bool> DlAdapter<T, Owner, R> {
    const NEW: Self = Self {
        link_ops: <RBTreeAtomicLink as DefaultLinkOps>::NEW,
        pointer_ops: OwnerPointerOps::new(),
    };
    const OFFSET: usize = if R {
        core::mem::offset_of!(SchedTask<T>, deadline.ready_link)
    } else {
        core::mem::offset_of!(SchedTask<T>, deadline.throttled_link)
    };
}

// SAFETY: each instantiation projects one actual `RBTreeAtomicLink` field of
// `SchedTask<T>`. `get_link` / `get_value` invert the same field offset inside
// the live entity allocation and do not change ownership. `OwnerPointerOps`
// holds each tree's independent `Arc`.
unsafe impl<T, Owner: SharedOwner<SchedTask<T>>, const R: bool> Adapter for DlAdapter<T, Owner, R> {
    type LinkOps = <RBTreeAtomicLink as DefaultLinkOps>::Ops;
    type PointerOps = OwnerPointerOps<SchedTask<T>, Owner>;

    /// # Safety
    ///
    /// `link` must have been produced by this adapter's `get_link` on a
    /// still-live `SchedTask<T>`.
    unsafe fn get_value(&self, link: NonNull<RBTreeAtomicLink>) -> *const SchedTask<T> {
        // SAFETY: Adapter requires a link previously projected by this
        // adapter; subtracting the same field offset recovers the live owning
        // allocation.
        unsafe { link.as_ptr().cast::<u8>().sub(Self::OFFSET).cast() }
    }

    /// # Safety
    ///
    /// `value` must point to a live, initialized `SchedTask<T>`.
    unsafe fn get_link(&self, value: *const SchedTask<T>) -> NonNull<RBTreeAtomicLink> {
        // SAFETY: Adapter supplies a live entity; OFFSET selects its aligned,
        // initialized link field without creating an exclusive entity
        // reference.
        let link = unsafe {
            value
                .cast::<u8>()
                .add(Self::OFFSET)
                .cast_mut()
                .cast::<RBTreeAtomicLink>()
        };
        // SAFETY: link is a non-null pointer to the field of a live `SchedTask<T>`, so it is safe to create a `NonNull` from it.
        unsafe { NonNull::new_unchecked(link) }
    }

    fn link_ops(&self) -> &Self::LinkOps {
        &self.link_ops
    }

    fn link_ops_mut(&mut self) -> &mut Self::LinkOps {
        &mut self.link_ops
    }

    fn pointer_ops(&self) -> &Self::PointerOps {
        &self.pointer_ops
    }
}

impl<'a, T, Owner: SharedOwner<SchedTask<T>>, const R: bool> KeyAdapter<'a>
    for DlAdapter<T, Owner, R>
{
    type Key = DlKey;

    fn get_key(&self, task: &'a SchedTask<T>) -> Self::Key {
        // SAFETY: `KeyAdapter::get_key` is only invoked by
        // `intrusive_collections` insert / find / cursor walks, which run from
        // `DeadlineClass` methods that already hold `RqLocked` from the
        // `Scheduler` entry. Linked keys stay immutable until the entity is
        // unlinked, and tree teardown never calls `get_key`.
        let tok = unsafe { RqLocked::assume() };
        let entity = task.deadline();
        let instant_ns = if R {
            entity.deadline_ns.get(tok)
        } else {
            entity.period_start_ns.get(tok)
        };
        (instant_ns, task.deadline().tree_id.get(tok))
    }
}

/// The run queue's runner, reduced to what per-run-queue admission needs.
///
/// The runner is linked in neither tree, so the class records it here instead
/// of asking its caller for `curr`. `id` is the runner's
/// [`DeadlineEntity::tree_id`], so `can_admit` can tell whether the task it is
/// about to change is this runner.
#[derive(Clone, Copy)]
struct DeadlineRunner {
    id: u64,
    ppm: u64,
}

/// Per-CPU CBS/EDF queue with per-run-queue bandwidth admission.
pub(crate) struct DeadlineClass<T, Owner: SharedOwner<SchedTask<T>>> {
    ready: RBTree<DlAdapter<T, Owner, true>>,
    throttled: RBTree<DlAdapter<T, Owner, false>>,
    /// Next tie-breaker handed out; mirrors `FairClass`'s counter.
    next_enqueue_id: u64,
    /// Runner while it belongs to this class, so admission still counts the
    /// reservation that `pick_next` took off both trees.
    running: Option<DeadlineRunner>,
}

impl<T, Owner: SharedOwner<SchedTask<T>>> DeadlineClass<T, Owner> {
    pub(crate) fn new() -> Self {
        Self {
            ready: RBTree::new(DlAdapter::<T, Owner, true>::NEW),
            throttled: RBTree::new(DlAdapter::<T, Owner, false>::NEW),
            next_enqueue_id: 0,
            running: None,
        }
    }

    /// Hands out the next tree tie-breaker.
    fn alloc_enqueue_id(&mut self) -> u64 {
        let id = self.next_enqueue_id;
        self.next_enqueue_id = self.next_enqueue_id.wrapping_add(1);
        id
    }

    /// Links `task` as ready under a fresh tie-breaker.
    fn insert_ready(&mut self, task: Owner, tok: RqLocked) {
        let id = self.alloc_enqueue_id();
        task.deadline().tree_id.set(id, tok);
        task.set_seat(QueueSeat::Deadline, tok);
        self.ready.insert(task);
    }

    /// Links `task` as throttled under a fresh tie-breaker.
    fn insert_throttled(&mut self, task: Owner, tok: RqLocked) {
        let id = self.alloc_enqueue_id();
        task.deadline().tree_id.set(id, tok);
        task.set_seat(QueueSeat::Deadline, tok);
        self.throttled.insert(task);
    }

    /// The task's current deadline parameters, if it is a Deadline task.
    ///
    /// `runtime_ns` / `period_ns` live only in `SchedAttr`; the entity stores
    /// the derived instants, so every derivation starts from the attribute.
    fn params(task: &SchedTask<T>, tok: RqLocked) -> Option<DlParams> {
        task.attr(tok).dl_params
    }

    /// Opens a fresh request starting at `now_ns`.
    fn replenish(&self, task: &SchedTask<T>, now_ns: u64, tok: RqLocked) {
        let Some(params) = Self::params(task, tok) else {
            return;
        };
        let entity = task.deadline();
        entity.budget_left_ns.set(params.runtime_ns, tok);
        entity
            .deadline_ns
            .set(now_ns.saturating_add(params.deadline_ns), tok);
        entity
            .period_start_ns
            .set(now_ns.saturating_add(params.period_ns), tok);
        entity.is_active.set(true, tok);
        entity.is_throttled.set(false, tok);
    }

    /// Reserves the bandwidth of the task's stored parameters.
    fn reserving_ppm(task: &SchedTask<T>, tok: RqLocked) -> u64 {
        Self::params(task, tok).map_or(0, DlParams::load_ppm)
    }

    /// Sum of the bandwidth reserved by every task linked in `tree`.
    fn tree_load_ppm<const R: bool>(tree: &RBTree<DlAdapter<T, Owner, R>>, tok: RqLocked) -> u64 {
        let mut cursor = tree.front();
        let mut total = 0u64;
        while let Some(task) = cursor.get() {
            total = total.saturating_add(Self::reserving_ppm(task, tok));
            cursor.move_next();
        }
        total
    }

    /// Reserved bandwidth of this run queue: both residency trees plus the
    /// runner.
    ///
    /// The runner is off both trees, so [`Self::running`] carries its
    /// reservation. A task that is off both trees *and* not the runner
    /// (blocked or migrating) is not counted: it holds its request but no
    /// longer competes for the queue's bandwidth.
    fn resident_load_ppm(&self, tok: RqLocked) -> u64 {
        Self::tree_load_ppm(&self.ready, tok)
            .saturating_add(Self::tree_load_ppm(&self.throttled, tok))
            .saturating_add(self.running.map_or(0, |runner| runner.ppm))
    }

    /// Whether [`Self::resident_load_ppm`] already counts `task`.
    fn is_counted(&self, task: &SchedTask<T>, tok: RqLocked) -> bool {
        task.seat(tok) == QueueSeat::Deadline
            || self
                .running
                .is_some_and(|runner| runner.id == task.deadline().tree_id.get(tok))
    }

    /// Records `task` as the runner, which leaves both trees.
    fn note_runner(&mut self, task: &SchedTask<T>, tok: RqLocked) {
        self.running = Some(DeadlineRunner {
            id: task.deadline().tree_id.get(tok),
            ppm: Self::reserving_ppm(task, tok),
        });
    }

    /// The runner left the CPU, this class, or the task lifetime.
    fn clear_runner(&mut self) {
        self.running = None;
    }
}

impl<T, Owner: SharedOwner<SchedTask<T>>> ClassOps<T, Owner> for DeadlineClass<T, Owner> {
    fn class(&self) -> SchedClass {
        SchedClass::Deadline
    }

    fn nr_queued(&self) -> usize {
        self.ready.iter().count()
    }

    fn has_runnable(&self) -> bool {
        !self.ready.is_empty()
    }

    fn advance(&mut self, clk: &RqClock, tok: RqLocked) {
        // Replenish every throttled task whose next period has started. Keys
        // are the period starts, so the earliest candidate is at the front and
        // the walk stops at the first task still waiting.
        while self
            .throttled
            .front()
            .get()
            .is_some_and(|task| task.deadline().period_start_ns.get(tok) <= clk.now_ns())
        {
            let Some(task) = self.throttled.front_mut().remove() else {
                break;
            };
            let Some(params) = Self::params(&task, tok) else {
                task.set_seat(QueueSeat::Detached, tok);
                continue;
            };
            let entity = task.deadline();
            entity.budget_left_ns.set(params.runtime_ns, tok);
            entity.is_throttled.set(false, tok);
            entity
                .deadline_ns
                .set(clk.now_ns().saturating_add(params.deadline_ns), tok);
            entity
                .period_start_ns
                .set(clk.now_ns().saturating_add(params.period_ns), tok);
            self.insert_ready(task, tok);
        }
    }

    fn enqueue(
        &mut self,
        task: Owner,
        kind: EnqueueKind,
        _curr: Option<&SchedTask<T>>,
        clk: &RqClock,
        tok: RqLocked,
    ) {
        match kind {
            // A first placement starts a fresh request.
            EnqueueKind::New => self.replenish(&task, clk.now_ns(), tok),
            // `Scheduler::set_attr` already opened the request through
            // `switched_to`, and a migrate-in is placed by its destination.
            EnqueueKind::ClassChange | EnqueueKind::MigrateIn => {}
            // Waking resumes the current request while budget remains;
            // otherwise the task stays throttled until its period starts.
            EnqueueKind::Wake { .. } => {
                if task.deadline().budget_left_ns.get(tok) == 0 {
                    task.deadline().is_throttled.set(true, tok);
                    self.insert_throttled(task, tok);
                    return;
                }
            }
        }
        self.insert_ready(task, tok);
    }

    fn dequeue(
        &mut self,
        task: &Owner,
        _curr: Option<&SchedTask<T>>,
        _clk: &RqClock,
        tok: RqLocked,
    ) -> Option<Owner> {
        if task.seat(tok) != QueueSeat::Deadline {
            return None;
        }
        if task.deadline().is_throttled.get(tok) {
            let key = (
                task.deadline().period_start_ns.get(tok),
                task.deadline().tree_id.get(tok),
            );
            let removed = self.throttled.find_mut(&key).remove();
            if removed.is_some() {
                task.set_seat(QueueSeat::Detached, tok);
            }
            return removed;
        }
        let key = (
            task.deadline().deadline_ns.get(tok),
            task.deadline().tree_id.get(tok),
        );
        let removed = self.ready.find_mut(&key).remove();
        if removed.is_some() {
            task.set_seat(QueueSeat::Detached, tok);
        }
        removed
    }

    fn steal(
        &mut self,
        _want: &mut dyn FnMut(&SchedTask<T>) -> bool,
        _curr: Option<&SchedTask<T>>,
        _clk: &RqClock,
        _tok: RqLocked,
    ) -> Option<Owner> {
        // Deadline tasks are pinned to their home run queue: bandwidth is
        // admitted and enforced per run queue, so stealing would move a task
        // outside the reservation that admitted it.
        None
    }

    fn pick_next(&mut self, _clk: &RqClock, tok: RqLocked) -> Option<Owner> {
        // The runner leaves the ready tree: `Scheduler::curr` owns it until
        // `put_prev` decides where it goes next, mirroring `FairClass`. Leaving
        // it linked would keep it counted by `nr_queued` (which feeds
        // `ktask`'s `nr_running`) and let it be picked twice.
        let key = {
            let task = self.ready.front().get()?;
            (
                task.deadline().deadline_ns.get(tok),
                task.deadline().tree_id.get(tok),
            )
        };
        let picked = self.ready.find_mut(&key).remove()?;
        picked.set_seat(QueueSeat::Detached, tok);
        self.note_runner(&picked, tok);
        Some(picked)
    }

    fn set_curr(&mut self, task: &SchedTask<T>, clk: &RqClock, tok: RqLocked) {
        // Boot or class-switch installs a runner that did not come from
        // `pick_next`. Only an already-open request keeps its state.
        if !task.deadline().is_active.get(tok) {
            self.replenish(task, clk.now_ns(), tok);
        }
        self.note_runner(task, tok);
        task.set_seat(QueueSeat::Detached, tok);
    }

    fn put_prev(
        &mut self,
        prev: Owner,
        reason: LeaveReason,
        _clk: &RqClock,
        tok: RqLocked,
    ) -> bool {
        // `leave` calls this only for this class's runner, so the recorded
        // runner is always the one leaving.
        self.clear_runner();
        let entity = prev.deadline();
        match reason {
            // The request continues: requeue into whichever residency matches
            // the remaining budget.
            LeaveReason::Yield | LeaveReason::Preempt => {
                if entity.is_throttled.get(tok) {
                    self.insert_throttled(prev, tok);
                } else {
                    self.insert_ready(prev, tok);
                }
                true
            }
            // Block and migrate keep the request alive but off any queue, so a
            // later enqueue resumes it. Exit drops the reservation.
            LeaveReason::Block | LeaveReason::Migrate => {
                prev.set_seat(QueueSeat::Detached, tok);
                false
            }
            LeaveReason::Exit => {
                entity.is_active.set(false, tok);
                entity.is_throttled.set(false, tok);
                prev.set_seat(QueueSeat::Detached, tok);
                false
            }
        }
    }

    fn account(
        &mut self,
        curr: &SchedTask<T>,
        elapsed_ns: u64,
        _clk: &RqClock,
        tok: RqLocked,
    ) -> bool {
        if elapsed_ns == 0 {
            return false;
        }
        let entity = curr.deadline();
        let budget = entity.budget_left_ns.get(tok);
        if budget == 0 {
            return false;
        }
        let left = budget.saturating_sub(elapsed_ns);
        entity.budget_left_ns.set(left, tok);
        if left == 0 {
            // Budget spent. The runner is not linked while it runs, so only
            // the state is recorded here; the following `put_prev` links it
            // into the throttled tree.
            entity.is_throttled.set(true, tok);
            return true;
        }
        false
    }

    fn tick_preempts(&mut self, _curr: &SchedTask<T>, _clk: &RqClock, _tok: RqLocked) -> bool {
        // An eligible peer with an earlier deadline is already reported
        // through `curr_timer_ns`, so the tick itself never preempts.
        false
    }

    fn wakeup_preempts(&mut self, curr: &SchedTask<T>, _clk: &RqClock, tok: RqLocked) -> bool {
        self.ready.front().get().is_some_and(|waiter| {
            waiter.deadline().deadline_ns.get(tok) < curr.deadline().deadline_ns.get(tok)
        })
    }

    fn curr_timer_ns(&self, curr: &SchedTask<T>, clk: &RqClock, tok: RqLocked) -> Option<u64> {
        let entity = curr.deadline();
        let now_ns = clk.now_ns();
        if entity.is_throttled.get(tok) {
            return Some(entity.period_start_ns.get(tok).saturating_sub(now_ns));
        }
        // While eligible the next look is the earlier of "budget runs out" and
        // "the request's deadline arrives".
        let budget_end_ns = now_ns.saturating_add(entity.budget_left_ns.get(tok));
        Some(
            budget_end_ns
                .min(entity.deadline_ns.get(tok))
                .saturating_sub(now_ns),
        )
    }

    fn class_timer_ns(&self, clk: &RqClock, tok: RqLocked) -> Option<u64> {
        let now_ns = clk.now_ns();
        let ready = self
            .ready
            .front()
            .get()
            .map(|task| task.deadline().deadline_ns.get(tok).saturating_sub(now_ns));
        let throttled = self.throttled.front().get().map(|task| {
            task.deadline()
                .period_start_ns
                .get(tok)
                .saturating_sub(now_ns)
        });
        match (ready, throttled) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }
    }

    fn attr_changed(
        &mut self,
        task: &Owner,
        _old: SchedAttr,
        is_curr: bool,
        _clk: &RqClock,
        tok: RqLocked,
    ) {
        // A new triple moves the request's keys, so the residency is rebuilt
        // under the fresh parameters. `Scheduler::set_attr` performs the
        // matching dequeue and re-enqueue.
        let entity = task.deadline();
        entity.is_active.set(false, tok);
        entity.is_throttled.set(false, tok);
        if is_curr {
            // `task.set_attr` already ran, so this records the new reservation.
            self.note_runner(task, tok);
        }
    }

    fn can_admit(
        &self,
        task: &SchedTask<T>,
        new: SchedAttr,
        tok: RqLocked,
    ) -> Result<(), SchedError> {
        let params = new.dl_params.ok_or(SchedError::InvalidPolicy)?;
        // The submission replaces the task's own reservation, so the old one is
        // only subtracted when this queue already counted it: a task in either
        // tree carries `QueueSeat::Deadline`, and the runner is
        // `self.running`. A blocked, migrating, Fair or fresh task was never
        // counted and contributes only its new value.
        let mut load = self.resident_load_ppm(tok);
        if self.is_counted(task, tok) {
            load = load.saturating_sub(Self::reserving_ppm(task, tok));
        }
        let limit = 1_000_000u64.saturating_sub(DL_BW_MARGIN_PPM);
        if load.saturating_add(params.load_ppm()) > limit {
            return Err(SchedError::AdmissionFull);
        }
        Ok(())
    }

    fn switched_from(&mut self, task: &SchedTask<T>, is_curr: bool, _clk: &RqClock, tok: RqLocked) {
        if is_curr {
            self.clear_runner();
        }
        // Leaving the class drops its reservation and any throttle state.
        let entity = task.deadline();
        entity.is_active.set(false, tok);
        entity.is_throttled.set(false, tok);
        entity.budget_left_ns.set(0, tok);
    }

    fn switched_to(&mut self, task: &SchedTask<T>, is_curr: bool, clk: &RqClock, tok: RqLocked) {
        // Entering the class opens a fresh request. `Scheduler::set_attr`
        // links the task afterwards when it is not the runner.
        self.replenish(task, clk.now_ns(), tok);
        if is_curr {
            self.note_runner(task, tok);
        }
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Earliest Eligible Virtual Deadline First (EEVDF).
//!
//! Each task tracks virtual runtime (`vruntime`) and a virtual deadline
//! `vd = ve + r/w` for its current request. Heavier weight slows `vruntime`,
//! so the task gets more CPU. System virtual time **V** is the load-weighted
//! average `vruntime` of runnable tasks. A task is eligible when
//! `vruntime ≤ V` — it has not run ahead of its share. Among eligible tasks,
//! the earliest deadline runs.
//!
//! Ready waiters live in two intrusive RB trees on the same
//! [`SchedTask`]: deadline order for pick, vruntime order for the eligible
//! prefix. Links are stored on the entity, so enqueue does not allocate an
//! index node.

use alloc::sync::Arc;
use core::ptr::NonNull;

use intrusive_collections::{
    Adapter, DefaultLinkOps, DefaultPointerOps, KeyAdapter, RBTree, RBTreeAtomicLink,
};

use crate::{
    class::{ClassOps, EnqueueKind, LeaveReason},
    clock::RqClock,
    entity::{QueueSeat, RqLocked, SchedTask},
    policy::{NICE_0_WEIGHT, SchedAttr, SchedClass, SchedError},
};

/// Ready-tree key: `(deadline, enqueue_id)`. Equal deadlines are FIFO.
type DeadlineKey = (i64, u64);

/// Convert wall-clock nanoseconds into virtual time.
///
/// Truncation keeps a request from shrinking below time already spent.
fn vruntime_delta(elapsed_ns: u64, weight: i64) -> i64 {
    if elapsed_ns == 0 || weight <= 0 {
        return 0;
    }
    let delta = elapsed_ns as i128 * NICE_0_WEIGHT as i128 / weight as i128;
    delta.clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

/// Convert a vruntime gap back to wall-clock nanoseconds at `weight`.
///
/// Rounds up so a timer does not fire before the gap has actually closed.
fn vruntime_to_wall_ns(delta_vruntime: i64, weight: i64) -> u64 {
    if delta_vruntime <= 0 || weight <= 0 {
        return 0;
    }
    let ns = (delta_vruntime as i128 * weight as i128 + (NICE_0_WEIGHT as i128 - 1))
        / NICE_0_WEIGHT as i128;
    ns.min(u64::MAX as i128) as u64
}

#[inline]
fn request_deadline(vruntime: i64, request_ns: u64, weight: i64) -> i64 {
    vruntime + vruntime_delta(request_ns, weight)
}

#[inline]
fn is_eligible(vruntime: i64, avg_vruntime: i64) -> bool {
    vruntime <= avg_vruntime
}

/// `intrusive_adapter!` cannot name `SchedTask`'s const-free generic, so the
/// two field projections share one adapter. `DEADLINE` selects the deadline
/// tree; otherwise the adapter indexes by vruntime.
struct FairAdapter<T, const DEADLINE: bool> {
    link_ops: <RBTreeAtomicLink as DefaultLinkOps>::Ops,
    pointer_ops: DefaultPointerOps<Arc<SchedTask<T>>>,
}

impl<T, const D: bool> FairAdapter<T, D> {
    const NEW: Self = Self {
        link_ops: <RBTreeAtomicLink as DefaultLinkOps>::NEW,
        pointer_ops: DefaultPointerOps::new(),
    };
    const OFFSET: usize = if D {
        core::mem::offset_of!(SchedTask<T>, fair.deadline_link)
    } else {
        core::mem::offset_of!(SchedTask<T>, fair.vruntime_link)
    };
}

// SAFETY: each adapter projects one actual `RBTreeAtomicLink` field of
// `SchedTask<T>`. `get_link` / `get_value` invert the same field offset
// inside the live `Arc` allocation and do not change ownership.
// `DefaultPointerOps` holds each tree's independent `Arc`.
unsafe impl<T, const D: bool> Adapter for FairAdapter<T, D> {
    type LinkOps = <RBTreeAtomicLink as DefaultLinkOps>::Ops;
    type PointerOps = DefaultPointerOps<Arc<SchedTask<T>>>;

    /// # Safety
    ///
    /// `link` must have been produced by this adapter's `get_link` on a
    /// still-live `SchedTask<T>`.
    unsafe fn get_value(&self, link: NonNull<RBTreeAtomicLink>) -> *const SchedTask<T> {
        // SAFETY: Adapter requires a link previously projected by this
        // adapter; subtracting the same field offset recovers the live
        // owning allocation.
        unsafe { link.as_ptr().cast::<u8>().sub(Self::OFFSET).cast() }
    }

    /// # Safety
    ///
    /// `value` must point to a live, initialized `SchedTask<T>`.
    unsafe fn get_link(&self, value: *const SchedTask<T>) -> NonNull<RBTreeAtomicLink> {
        // SAFETY: Adapter supplies a live entity; OFFSET selects its
        // aligned, initialized link field without creating an exclusive
        // entity reference.
        unsafe { NonNull::new_unchecked(value.cast::<u8>().add(Self::OFFSET).cast_mut().cast()) }
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

impl<'a, T, const D: bool> KeyAdapter<'a> for FairAdapter<T, D> {
    type Key = DeadlineKey;

    fn get_key(&self, task: &'a SchedTask<T>) -> Self::Key {
        // SAFETY: `KeyAdapter::get_key` is only invoked by
        // `intrusive_collections` 0.10.2 insert / find / range / cursor
        // walks. Those run from `FairClass` methods that already hold
        // `RqLocked` from the Scheduler entry. Linked sort keys are
        // immutable until the entity is unlinked.
        //
        // Tree teardown does not call `get_key`: `RBTree::drop` is
        // `clear()`, and `clear_recurse` only walks `left`/`right`,
        // `release_link`, and `PointerOps::from_raw`. Cursor types have
        // no `Drop` impl. So a non-empty `Scheduler` drop without the
        // RQ lock does not race on `RqCell`. Atomic keys are unnecessary.
        let tok = unsafe { RqLocked::assume() };
        if D {
            (task.fair().deadline.get(tok), task.fair().tree_id.get(tok))
        } else {
            (task.fair().vruntime.get(tok), task.fair().tree_id.get(tok))
        }
    }
}

/// Per-CPU EEVDF ready queue.
///
/// Waiters sit in two intrusive trees on the same entity: deadline order
/// for pick, vruntime order for the eligible prefix. The runner stays off
/// both trees; pick and preemption take it as `curr` when **V** must
/// include it. Putting the runner back before a preemption probe lets the
/// two swap on deadline repeatedly.
pub(crate) struct FairClass<T> {
    ready: RBTree<FairAdapter<T, true>>,
    vruntime_tree: RBTree<FairAdapter<T, false>>,
    /// Cached minima: `RBTree::front` itself is O(log N).
    first_deadline: Option<Arc<SchedTask<T>>>,
    first_vruntime: Option<Arc<SchedTask<T>>>,
    queued: usize,
    /// Placement floor. Includes the off-tree runner so an ahead waiter
    /// cannot freeze this above the real average.
    min_vruntime: i64,
    /// Running `Σ weight × vruntime` for an O(1) average. Widened to i128
    /// because the product of two i64 fields does not fit in i64.
    sum_wv: i128,
    total_weight: i64,
    /// Preferred successor at the next pick, if still eligible.
    next_buddy: Option<Arc<SchedTask<T>>>,
    /// The waker is about to sleep. An eligible buddy may take the CPU even
    /// with a later deadline. Cleared when that buddy is picked, or when the
    /// tree is empty; a failed preemption probe must not clear it.
    has_sync_wake: bool,
    next_enqueue_id: u64,
    /// RQ-wide request length, applied to `New` entities.
    request_ns: u64,
}

impl<T> FairClass<T> {
    pub const fn new() -> Self {
        Self {
            ready: RBTree::new(FairAdapter::NEW),
            vruntime_tree: RBTree::new(FairAdapter::NEW),
            first_deadline: None,
            first_vruntime: None,
            queued: 0,
            min_vruntime: 0,
            sum_wv: 0,
            total_weight: 0,
            next_buddy: None,
            has_sync_wake: false,
            next_enqueue_id: 0,
            request_ns: crate::DEFAULT_SLICE_NS,
        }
    }

    #[inline]
    fn weight(task: &SchedTask<T>, tok: RqLocked) -> i64 {
        i64::from(task.fair().weight.get(tok))
    }

    fn deadline_key(task: &SchedTask<T>, tok: RqLocked) -> DeadlineKey {
        (task.fair().deadline.get(tok), task.fair().tree_id.get(tok))
    }

    fn alloc_enqueue_id(&mut self) -> u64 {
        let id = self.next_enqueue_id;
        self.next_enqueue_id = self.next_enqueue_id.wrapping_add(1);
        id
    }

    /// Load-weighted mean vruntime **V**. `curr` is the off-tree runner.
    fn avg_vruntime(&self, curr: Option<&SchedTask<T>>, tok: RqLocked) -> i64 {
        let Some(curr) = curr else {
            return if self.total_weight <= 0 {
                self.min_vruntime
            } else {
                (self.sum_wv / self.total_weight as i128) as i64
            };
        };
        let weight = Self::weight(curr, tok);
        let total = self.total_weight as i128 + weight as i128;
        if total <= 0 {
            self.min_vruntime
        } else {
            let sum = self.sum_wv + curr.fair().vruntime.get(tok) as i128 * weight as i128;
            (sum / total) as i64
        }
    }

    fn load(&self, curr: Option<&SchedTask<T>>, tok: RqLocked) -> i64 {
        match curr {
            Some(curr) => self.total_weight + Self::weight(curr, tok),
            None => self.total_weight,
        }
    }

    /// Open a request of `request_ns` at `vruntime` (`vd = ve + r/w`).
    fn set_request(task: &SchedTask<T>, vruntime: i64, request_ns: u64, tok: RqLocked) {
        task.fair().vruntime.set(vruntime, tok);
        task.fair().slice_left_ns.set(request_ns, tok);
        task.fair().deadline.set(
            request_deadline(vruntime, request_ns, Self::weight(task, tok)),
            tok,
        );
    }

    fn clamp_vlag(lag: i64, weight: i64, request_ns: u64) -> i64 {
        let limit = vruntime_delta(request_ns.saturating_mul(2).max(1), weight);
        lag.clamp(-limit, limit)
    }

    fn place_new_entity(&self, task: &SchedTask<T>, curr: Option<&SchedTask<T>>, tok: RqLocked) {
        let vruntime = self.avg_vruntime(curr, tok);
        task.fair().request_ns.set(self.request_ns, tok);
        task.fair().vlag.set(0, tok);
        Self::set_request(task, vruntime, self.request_ns, tok);
    }

    /// PLACE_LAG: `vruntime = V − lag × (W+w)/W`.
    ///
    /// Lag is inflated by the joining weight so a task does not dump its
    /// whole credit the instant it lands on a loaded queue.
    fn place_entity(&self, task: &SchedTask<T>, avg: i64, load: i64, tok: RqLocked) -> i64 {
        if load <= 0 {
            return self.min_vruntime;
        }
        let weight = Self::weight(task, tok);
        let request_ns = task.fair().request_ns.get(tok);
        let lag = Self::clamp_vlag(task.fair().vlag.get(tok), weight, request_ns) as i128;
        let inflated = lag * (load as i128 + weight as i128) / load as i128;
        (avg as i128 - inflated) as i64
    }

    fn apply_lag(&self, task: &SchedTask<T>, curr: Option<&SchedTask<T>>, tok: RqLocked) {
        let vruntime = self.place_entity(
            task,
            self.avg_vruntime(curr, tok),
            self.load(curr, tok),
            tok,
        );
        Self::set_request(task, vruntime, task.fair().request_ns.get(tok), tok);
    }

    fn update_min_vruntime(&mut self, curr: Option<&SchedTask<T>>, tok: RqLocked) {
        let tree_min = self
            .first_vruntime
            .as_ref()
            .map(|task| task.fair().vruntime.get(tok));
        let candidate = match (curr, tree_min) {
            (Some(curr), Some(tree_min)) => curr.fair().vruntime.get(tok).min(tree_min),
            (Some(curr), None) => curr.fair().vruntime.get(tok),
            (None, Some(tree_min)) => tree_min,
            (None, None) => self.min_vruntime,
        };
        self.min_vruntime = self.min_vruntime.max(candidate);
    }

    fn first_deadline_key(&self, tok: RqLocked) -> Option<DeadlineKey> {
        self.first_deadline
            .as_ref()
            .map(|task| Self::deadline_key(task, tok))
    }

    fn contains(&self, task: &SchedTask<T>, tok: RqLocked) -> bool {
        self.ready
            .find(&Self::deadline_key(task, tok))
            .get()
            .is_some_and(|found| core::ptr::eq(found, task))
    }

    fn enqueue_tree(
        &mut self,
        task: Arc<SchedTask<T>>,
        curr: Option<&SchedTask<T>>,
        tok: RqLocked,
    ) {
        let vruntime = task.fair().vruntime.get(tok);
        let id = self.alloc_enqueue_id();
        task.fair().tree_id.set(id, tok);
        let deadline = task.fair().deadline.get(tok);
        let weight = Self::weight(&task, tok);
        task.set_seat(QueueSeat::from_sched_class(SchedClass::Fair), tok);
        debug_assert!(!task.fair().deadline_link.is_linked());
        debug_assert!(!task.fair().vruntime_link.is_linked());
        self.ready.insert(task.clone());
        self.vruntime_tree.insert(task.clone());
        if self
            .first_deadline
            .as_ref()
            .is_none_or(|first| (deadline, id) < Self::deadline_key(first, tok))
        {
            self.first_deadline = Some(task.clone());
        }
        if self.first_vruntime.as_ref().is_none_or(|first| {
            (vruntime, id)
                < (
                    first.fair().vruntime.get(tok),
                    first.fair().tree_id.get(tok),
                )
        }) {
            self.first_vruntime = Some(task);
        }
        self.queued += 1;
        self.sum_wv += vruntime as i128 * weight as i128;
        self.total_weight += weight;
        if curr.is_some() {
            self.update_min_vruntime(curr, tok);
        }
    }

    fn dequeue_entity(&mut self, key: DeadlineKey, tok: RqLocked) -> Option<Arc<SchedTask<T>>> {
        let mut cursor = self.ready.find_mut(&key);
        let task = cursor.remove()?;
        if self
            .first_deadline
            .as_ref()
            .is_some_and(|first| Arc::ptr_eq(first, &task))
        {
            self.first_deadline = cursor.as_cursor().clone_pointer();
        }
        let id = task.fair().tree_id.get(tok);
        let weight = Self::weight(&task, tok);
        let vruntime = task.fair().vruntime.get(tok);
        let mut cursor = self.vruntime_tree.find_mut(&(vruntime, id));
        assert!(
            cursor
                .get()
                .is_some_and(|found| core::ptr::eq(found, Arc::as_ptr(&task))),
            "EEVDF vruntime tree out of sync with ready for task {id}"
        );
        let secondary = cursor.remove().unwrap();
        if self
            .first_vruntime
            .as_ref()
            .is_some_and(|first| Arc::ptr_eq(first, &task))
        {
            self.first_vruntime = cursor.as_cursor().clone_pointer();
        }
        drop(secondary);
        task.set_seat(QueueSeat::Detached, tok);
        self.queued = self.queued.saturating_sub(1);
        self.sum_wv -= vruntime as i128 * weight as i128;
        self.total_weight -= weight;
        Some(task)
    }

    fn earliest_eligible(&self, avg: i64, tok: RqLocked) -> Option<DeadlineKey> {
        let first = self.first_deadline.as_ref()?;
        if is_eligible(first.fair().vruntime.get(tok), avg) {
            return Some(Self::deadline_key(first, tok));
        }
        // PLACE_LAG negative lag parks ineligible tasks at the deadline
        // front, so walking deadline order would be O(n). Scan the
        // vruntime eligible prefix instead.
        self.vruntime_tree
            .iter()
            .take_while(|task| is_eligible(task.fair().vruntime.get(tok), avg))
            .map(|task| Self::deadline_key(task, tok))
            .min()
    }

    fn clear_buddy(&mut self) {
        self.next_buddy = None;
        self.has_sync_wake = false;
    }

    fn clear_buddy_if(&mut self, task: &SchedTask<T>) {
        if self
            .next_buddy
            .as_ref()
            .is_some_and(|buddy| core::ptr::eq(buddy.as_ref(), task))
        {
            self.clear_buddy();
        }
    }

    fn is_buddy_eligible(&self, avg: i64, tok: RqLocked) -> bool {
        self.next_buddy
            .as_ref()
            .is_some_and(|buddy| is_eligible(buddy.fair().vruntime.get(tok), avg))
    }

    fn entity_before(a: &SchedTask<T>, b: &SchedTask<T>, tok: RqLocked) -> bool {
        let a_deadline = a.fair().deadline.get(tok);
        let b_deadline = b.fair().deadline.get(tok);
        a_deadline < b_deadline
            || (a_deadline == b_deadline && a.fair().tree_id.get(tok) < b.fair().tree_id.get(tok))
    }

    fn set_next_buddy(&mut self, wakee: Arc<SchedTask<T>>, tok: RqLocked) {
        if let Some(existing) = self.next_buddy.as_ref()
            && Self::entity_before(existing, &wakee, tok)
        {
            return;
        }
        self.next_buddy = Some(wakee);
    }

    /// Prefer the nominated wakee if it is eligible. `has_sync_wake` is a
    /// preemption hint only; async wakes still nominate and must win pick.
    fn pick_buddy(&mut self, tok: RqLocked) -> Option<Arc<SchedTask<T>>> {
        let avg = self.avg_vruntime(None, tok);
        if !self.is_buddy_eligible(avg, tok) {
            return None;
        }
        let buddy = self.next_buddy.take()?;
        self.has_sync_wake = false;
        Some(
            self.dequeue_entity(Self::deadline_key(&buddy, tok), tok)
                .expect("nominated buddy is in the ready tree"),
        )
    }

    fn update_entity_lag(&self, curr: &SchedTask<T>, tok: RqLocked) {
        let lag = self.avg_vruntime(Some(curr), tok) - curr.fair().vruntime.get(tok);
        curr.fair().vlag.set(
            Self::clamp_vlag(
                lag,
                Self::weight(curr, tok),
                curr.fair().request_ns.get(tok),
            ),
            tok,
        );
    }

    fn rescale_vlag(task: &SchedTask<T>, old_weight: i64, tok: RqLocked) {
        let new_weight = Self::weight(task, tok);
        if new_weight != old_weight && new_weight != 0 {
            let scaled =
                task.fair().vlag.get(tok) as i128 * old_weight as i128 / new_weight as i128;
            task.fair().vlag.set(scaled as i64, tok);
        }
    }

    /// Renew the request when it is exhausted. Returns whether a waiter should run.
    fn update_deadline(&mut self, curr: &SchedTask<T>, tok: RqLocked) -> bool {
        let is_done = curr.fair().vruntime.get(tok) >= curr.fair().deadline.get(tok)
            || curr.fair().slice_left_ns.get(tok) == 0;
        if !is_done {
            return false;
        }
        Self::set_request(
            curr,
            curr.fair().vruntime.get(tok),
            curr.fair().request_ns.get(tok),
            tok,
        );
        !self.ready.is_empty()
    }

    fn dequeue_with_lag(
        &mut self,
        task: &SchedTask<T>,
        curr: Option<&SchedTask<T>>,
        tok: RqLocked,
    ) -> Option<Arc<SchedTask<T>>> {
        if !self.contains(task, tok) {
            return None;
        }
        self.clear_buddy_if(task);
        let lag = self.avg_vruntime(curr, tok) - task.fair().vruntime.get(tok);
        let removed = self.dequeue_entity(Self::deadline_key(task, tok), tok)?;
        removed.fair().vlag.set(
            Self::clamp_vlag(
                lag,
                Self::weight(&removed, tok),
                removed.fair().request_ns.get(tok),
            ),
            tok,
        );
        self.update_min_vruntime(curr, tok);
        Some(removed)
    }

    fn enqueue_wake(
        &mut self,
        task: Arc<SchedTask<T>>,
        curr: Option<&SchedTask<T>>,
        sync: bool,
        tok: RqLocked,
    ) {
        self.apply_lag(&task, curr, tok);
        let wakee = task.clone();
        self.enqueue_tree(task, curr, tok);
        self.set_next_buddy(wakee, tok);
        if sync && self.next_buddy.is_some() {
            self.has_sync_wake = true;
        }
    }

    fn requeue_prev(&mut self, prev: Arc<SchedTask<T>>, tok: RqLocked) {
        self.enqueue_tree(prev, None, tok);
    }

    fn peer_preempts(&mut self, curr: &SchedTask<T>, tok: RqLocked) -> bool {
        if self.ready.is_empty() {
            return false;
        }
        let avg = self.avg_vruntime(Some(curr), tok);
        if self.has_sync_wake && self.is_buddy_eligible(avg, tok) {
            return true;
        }
        let Some(key) = self.earliest_eligible(avg, tok) else {
            return false;
        };
        let peer = self
            .ready
            .find(&key)
            .get()
            .expect("eligible key is in ready");
        !is_eligible(curr.fair().vruntime.get(tok), avg) || !Self::entity_before(curr, peer, tok)
    }

    fn is_unplaced(task: &SchedTask<T>, tok: RqLocked) -> bool {
        let fair = task.fair();
        fair.vlag.get(tok) == 0 && fair.vruntime.get(tok) == 0 && fair.deadline.get(tok) == 0
    }

    fn rescale_deadline(task: &SchedTask<T>, old_weight: i64, tok: RqLocked) {
        Self::rescale_vlag(task, old_weight, tok);
        let remaining_ns = task.fair().slice_left_ns.get(tok).max(1);
        task.fair().deadline.set(
            request_deadline(
                task.fair().vruntime.get(tok),
                remaining_ns,
                Self::weight(task, tok),
            ),
            tok,
        );
    }
}

impl<T> ClassOps<T> for FairClass<T> {
    fn class(&self) -> SchedClass {
        SchedClass::Fair
    }

    fn nr_queued(&self) -> usize {
        self.queued
    }

    fn has_runnable(&self) -> bool {
        !self.ready.is_empty()
    }

    fn advance(&mut self, _clk: &RqClock, _tok: RqLocked) {}

    fn enqueue(
        &mut self,
        task: Arc<SchedTask<T>>,
        kind: EnqueueKind,
        curr: Option<&SchedTask<T>>,
        _clk: &RqClock,
        tok: RqLocked,
    ) {
        match kind {
            EnqueueKind::New => {
                self.place_new_entity(&task, curr, tok);
                self.enqueue_tree(task, curr, tok);
            }
            EnqueueKind::Wake { sync } => self.enqueue_wake(task, curr, sync, tok),
            EnqueueKind::MigrateIn => self.enqueue_wake(task, curr, false, tok),
            EnqueueKind::ClassChange => {
                // Keep the lag saved by dequeue / switched_from. Do not
                // nominate a buddy on setattr.
                self.apply_lag(&task, curr, tok);
                self.enqueue_tree(task, curr, tok);
            }
        }
    }

    fn dequeue(
        &mut self,
        task: &SchedTask<T>,
        curr: Option<&SchedTask<T>>,
        _clk: &RqClock,
        tok: RqLocked,
    ) -> Option<Arc<SchedTask<T>>> {
        self.dequeue_with_lag(task, curr, tok)
    }

    fn steal(
        &mut self,
        want: &mut dyn FnMut(&SchedTask<T>) -> bool,
        curr: Option<&SchedTask<T>>,
        _clk: &RqClock,
        tok: RqLocked,
    ) -> Option<Arc<SchedTask<T>>> {
        let mut cursor = self.ready.front();
        let task = loop {
            match cursor.get() {
                Some(queued) if want(queued) => break cursor.clone_pointer()?,
                Some(_) => cursor.move_next(),
                None => return None,
            }
        };
        self.dequeue_with_lag(&task, curr, tok)
    }

    fn pick_next(&mut self, _clk: &RqClock, tok: RqLocked) -> Option<Arc<SchedTask<T>>> {
        if self.ready.is_empty() {
            self.clear_buddy();
            return None;
        }
        if let Some(picked) = self.pick_buddy(tok) {
            return Some(picked);
        }
        let avg = self.avg_vruntime(None, tok);
        let key = match self.earliest_eligible(avg, tok) {
            Some(key) => key,
            None => self
                .first_deadline_key(tok)
                .expect("ready tree is not empty"),
        };
        let picked = self
            .dequeue_entity(key, tok)
            .expect("pick key is in the ready tree");
        self.clear_buddy_if(&picked);
        Some(picked)
    }

    fn set_curr(&mut self, task: &SchedTask<T>, _clk: &RqClock, tok: RqLocked) {
        // Constructor zeros mean the entity was never placed. A runner that
        // already has saved lag must not be re-placed as new.
        if Self::is_unplaced(task, tok) {
            self.place_new_entity(task, None, tok);
        }
        self.update_min_vruntime(Some(task), tok);
    }

    fn put_prev(
        &mut self,
        prev: Arc<SchedTask<T>>,
        reason: LeaveReason,
        _clk: &RqClock,
        tok: RqLocked,
    ) -> bool {
        self.clear_buddy_if(&prev);
        self.update_min_vruntime(Some(&prev), tok);
        match reason {
            LeaveReason::Yield => {
                let vruntime = prev.fair().vruntime.get(tok).max(self.min_vruntime);
                Self::set_request(&prev, vruntime, prev.fair().request_ns.get(tok), tok);
                self.requeue_prev(prev, tok);
                true
            }
            LeaveReason::Preempt => {
                let vruntime = prev.fair().vruntime.get(tok).max(self.min_vruntime);
                prev.fair().vruntime.set(vruntime, tok);
                let slice_left_ns = prev.fair().slice_left_ns.get(tok);
                if slice_left_ns == 0 {
                    Self::set_request(&prev, vruntime, prev.fair().request_ns.get(tok), tok);
                } else if prev.fair().deadline.get(tok) <= vruntime {
                    prev.fair().deadline.set(
                        request_deadline(vruntime, slice_left_ns, Self::weight(&prev, tok)),
                        tok,
                    );
                }
                self.requeue_prev(prev, tok);
                true
            }
            LeaveReason::Block | LeaveReason::Migrate => {
                // Both deactivate without requeue and arm PLACE_LAG. Migrate
                // stays its own reason so a sleep-only path cannot treat a
                // task that is about to run on another CPU as blocked.
                self.update_entity_lag(&prev, tok);
                false
            }
            LeaveReason::Exit => false,
        }
    }

    fn account(
        &mut self,
        curr: &SchedTask<T>,
        elapsed_ns: u64,
        _clk: &RqClock,
        tok: RqLocked,
    ) -> bool {
        if elapsed_ns > 0 {
            let delta = vruntime_delta(elapsed_ns, Self::weight(curr, tok));
            curr.fair()
                .vruntime
                .update(tok, |v| v.saturating_add(delta));
            let slice_left_ns = curr.fair().slice_left_ns.get(tok);
            curr.fair().slice_left_ns.set(
                slice_left_ns.saturating_sub(elapsed_ns.min(slice_left_ns)),
                tok,
            );
        }
        self.update_deadline(curr, tok)
    }

    fn tick_preempts(&mut self, curr: &SchedTask<T>, _clk: &RqClock, tok: RqLocked) -> bool {
        self.peer_preempts(curr, tok)
    }

    fn wakeup_preempts(&mut self, curr: &SchedTask<T>, _clk: &RqClock, tok: RqLocked) -> bool {
        // Same body as `tick_preempts` today. Keep the mouth so a later wake
        // probe can refuse to clear `has_sync_wake` independently of tick.
        self.peer_preempts(curr, tok)
    }

    fn curr_timer_ns(&self, curr: &SchedTask<T>, _clk: &RqClock, tok: RqLocked) -> Option<u64> {
        if self.ready.is_empty() {
            return None;
        }
        if curr.fair().deadline.get(tok) <= curr.fair().vruntime.get(tok) {
            return Some(0);
        }
        let mut until_ns = curr.fair().slice_left_ns.get(tok);
        let avg = self.avg_vruntime(Some(curr), tok);
        let load = self.load(Some(curr), tok);
        // Until the WF_SYNC buddy becomes eligible. Do not poll every
        // ineligible waiter at the hrtick floor — that livelocks busy CPUs
        // after NEXT_BUDDY. **V** advances at NICE_0_WEIGHT / W.
        if self.has_sync_wake
            && let Some(buddy) = self.next_buddy.as_ref()
            && !is_eligible(buddy.fair().vruntime.get(tok), avg)
            && load > 0
        {
            until_ns = until_ns.min(vruntime_to_wall_ns(
                buddy.fair().vruntime.get(tok) - avg,
                load,
            ));
        }
        Some(until_ns)
    }

    fn class_timer_ns(&self, _clk: &RqClock, _tok: RqLocked) -> Option<u64> {
        None
    }

    fn attr_changed(
        &mut self,
        task: &Arc<SchedTask<T>>,
        old: SchedAttr,
        is_curr: bool,
        _clk: &RqClock,
        tok: RqLocked,
    ) {
        let old_weight = i64::from(old.nice.weight());
        if is_curr {
            Self::rescale_deadline(task, old_weight, tok);
            return;
        }
        if task.seat(tok) != QueueSeat::Fair {
            Self::rescale_vlag(task, old_weight, tok);
            return;
        }
        if let Some(removed) = self.dequeue_entity(Self::deadline_key(task, tok), tok) {
            Self::rescale_deadline(&removed, old_weight, tok);
            self.enqueue_tree(removed, None, tok);
        }
    }

    fn can_admit(
        &self,
        _task: &SchedTask<T>,
        _new: SchedAttr,
        _tok: RqLocked,
    ) -> Result<(), SchedError> {
        Ok(())
    }

    fn switched_from(&mut self, task: &SchedTask<T>, is_curr: bool, _clk: &RqClock, tok: RqLocked) {
        if is_curr {
            self.update_entity_lag(task, tok);
            self.update_min_vruntime(None, tok);
        }
    }

    fn switched_to(&mut self, task: &SchedTask<T>, is_curr: bool, clk: &RqClock, tok: RqLocked) {
        if is_curr {
            // Restore the lag saved by switched_from; do not place as new.
            self.set_curr(task, clk, tok);
        }
    }
}

#[cfg(unittest)]
impl<T> FairClass<T> {
    pub(crate) fn min_vruntime_for_test(&self) -> i64 {
        self.min_vruntime
    }

    pub(crate) fn avg_vruntime_for_test(&self, curr: Option<&SchedTask<T>>) -> i64 {
        // SAFETY: kernel unittests run on one thread and own this class.
        unsafe { self.avg_vruntime(curr, RqLocked::assume()) }
    }

    pub(crate) fn inject_ready_for_test(
        &mut self,
        task: Arc<SchedTask<T>>,
        curr: Option<&SchedTask<T>>,
    ) {
        // SAFETY: kernel unittests run on one thread and own this class.
        let tok = unsafe { RqLocked::assume() };
        self.enqueue_tree(task, curr, tok);
    }

    pub(crate) fn has_sync_wake_for_test(&self) -> bool {
        self.has_sync_wake
    }
}

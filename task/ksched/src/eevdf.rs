// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
use core::{
    ops::Deref,
    sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, Ordering},
};

use crate::BaseScheduler;

const NICE_0_WEIGHT: i128 = 1024;

/// Linux-compatible nice-to-weight table.
/// Index 0 corresponds to nice -20; index 39 to nice 19.
const NICE_TO_WEIGHT: [isize; 40] = [
    // -20
    88761, 71755, 56483, 46273, 36291, // -15
    29154, 23254, 18705, 14949, 11916, // -10
    9548, 7620, 6100, 4904, 3906, // -5
    3121, 2501, 1991, 1586, 1277, // 0
    1024, 820, 655, 526, 423, // 5
    335, 272, 215, 172, 137, // 10
    110, 87, 70, 56, 45, // 15
    36, 29, 23, 18, 15,
];

fn nice_to_weight(nice: isize) -> isize {
    NICE_TO_WEIGHT[(nice + 20).clamp(0, 39) as usize]
}

/// Per-tick vruntime delta: `NICE_0_WEIGHT² / weight`.
/// Higher weight ⇒ smaller delta ⇒ slower vruntime growth ⇒ more CPU share.
fn vruntime_delta(weight: isize) -> isize {
    (NICE_0_WEIGHT * NICE_0_WEIGHT / weight as i128) as isize
}

/// Deadline increment for a request of `ticks` wall-clock ticks:
/// `ticks × NICE_0_WEIGHT² / weight`.
fn deadline_delta(ticks: usize, weight: isize) -> isize {
    (ticks as i128 * NICE_0_WEIGHT * NICE_0_WEIGHT / weight as i128) as isize
}

/// Per-task EEVDF scheduling entity.
///
/// Wraps an inner value `T` with scheduling metadata: virtual runtime,
/// virtual deadline, virtual lag, nice value, remaining/request time-slice,
/// and a monotonic id used as tie-breaker in the deadline-ordered ready queue.
pub struct EevdfEntity<T, const MAX_TIME_SLICE: usize> {
    inner: T,
    vruntime: AtomicIsize,
    deadline: AtomicIsize,
    /// Virtual lag `V - vruntime` saved when leaving the run queue (sleep).
    vlag: AtomicIsize,
    /// When set, the next [`EevdfScheduler::put_prev_task`] applies lag-based
    /// placement (wake / migrate-in after dequeue), not yield/preempt requeue.
    needs_place: AtomicBool,
    nice: AtomicIsize,
    /// Remaining wall-clock ticks in the current request.
    slice: AtomicIsize,
    /// Full request length in ticks (defaults to `MAX_TIME_SLICE`).
    request: AtomicIsize,
    id: AtomicU64,
}

impl<T, const S: usize> EevdfEntity<T, S> {
    pub const fn new(inner: T) -> Self {
        Self {
            inner,
            vruntime: AtomicIsize::new(0),
            deadline: AtomicIsize::new(0),
            vlag: AtomicIsize::new(0),
            needs_place: AtomicBool::new(false),
            nice: AtomicIsize::new(0),
            slice: AtomicIsize::new(S as isize),
            request: AtomicIsize::new(S as isize),
            id: AtomicU64::new(0),
        }
    }

    fn weight(&self) -> isize {
        nice_to_weight(self.nice.load(Ordering::Acquire))
    }

    fn vruntime(&self) -> isize {
        self.vruntime.load(Ordering::Acquire)
    }

    fn set_vruntime(&self, v: isize) {
        self.vruntime.store(v, Ordering::Release);
    }

    pub(crate) fn deadline(&self) -> isize {
        self.deadline.load(Ordering::Acquire)
    }

    fn set_deadline(&self, d: isize) {
        self.deadline.store(d, Ordering::Release);
    }

    fn vlag(&self) -> isize {
        self.vlag.load(Ordering::Acquire)
    }

    fn set_vlag(&self, lag: isize) {
        self.vlag.store(lag, Ordering::Release);
    }

    fn needs_place(&self) -> bool {
        self.needs_place.load(Ordering::Acquire)
    }

    fn set_needs_place(&self, needed: bool) {
        self.needs_place.store(needed, Ordering::Release);
    }

    /// Accessors only used in unit tests to construct or inspect edge-case scenarios.
    #[cfg(any(test, unittest))]
    pub(crate) fn set_deadline_for_test(&self, d: isize) {
        self.set_deadline(d);
    }

    #[cfg(any(test, unittest))]
    pub(crate) fn set_vruntime_for_test(&self, v: isize) {
        self.set_vruntime(v);
    }

    #[cfg(any(test, unittest))]
    pub(crate) fn vruntime_for_test(&self) -> isize {
        self.vruntime()
    }

    #[cfg(any(test, unittest))]
    pub(crate) fn vlag_for_test(&self) -> isize {
        self.vlag()
    }

    #[cfg(any(test, unittest))]
    pub(crate) fn slice_for_test(&self) -> isize {
        self.slice()
    }

    #[cfg(any(test, unittest))]
    pub(crate) fn needs_place_for_test(&self) -> bool {
        self.needs_place()
    }

    #[cfg(any(test, unittest))]
    pub(crate) fn set_vlag_for_test(&self, lag: isize) {
        self.set_vlag(lag);
    }

    #[cfg(any(test, unittest))]
    pub(crate) fn set_needs_place_for_test(&self, needed: bool) {
        self.set_needs_place(needed);
    }

    fn id(&self) -> u64 {
        self.id.load(Ordering::Acquire)
    }

    fn set_id(&self, id: u64) {
        self.id.store(id, Ordering::Release);
    }

    pub(crate) fn slice(&self) -> isize {
        self.slice.load(Ordering::Acquire)
    }

    fn request_ticks(&self) -> usize {
        self.request.load(Ordering::Acquire).max(1) as usize
    }

    /// Sets the request length used for new virtual deadlines.
    ///
    /// `ticks` must be in `1..=MAX_TIME_SLICE`. Shorter requests get earlier
    /// deadlines and are preferred among eligible tasks (Linux-style latency).
    pub fn set_request_ticks(&self, ticks: usize) -> bool {
        if !(1..=S).contains(&ticks) {
            return false;
        }
        self.request.store(ticks as isize, Ordering::Release);
        true
    }

    pub(crate) fn reset_slice(&self) {
        self.slice
            .store(self.request.load(Ordering::Acquire), Ordering::Release);
    }

    #[allow(dead_code)]
    /// Decrements the time-slice counter by one and returns the old value.
    /// Used by simple RR/FIFO adapters in [`crate::per_cpu`].
    pub(crate) fn fetch_sub_slice(&self) -> isize {
        self.slice.fetch_sub(1, Ordering::Release)
    }

    pub const fn inner(&self) -> &T {
        &self.inner
    }
}

impl<T, const S: usize> Deref for EevdfEntity<T, S> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EevdfStats {
    pub picks_total: u64,
    pub preempt_by_deadline: u64,
    pub fallback_no_eligible: u64,
    pub slice_expired: u64,
}

/// Per-task EEVDF (Earliest Eligible Virtual Deadline First) scheduler.
///
/// Each task carries virtual runtime (`vruntime`), virtual deadline, and
/// virtual lag (`vlag`). At every scheduling decision the scheduler computes
/// the system virtual time **V** (load-weighted average vruntime of runnable
/// tasks) and picks the task with the smallest deadline among those with
/// `vruntime ≤ V` (eligible / non-negative lag).
///
/// Unlike Linux CFS (where `curr` stays on the runqueue), this implementation
/// dequeues the running task from the ready queue. [`Self::curr`] tracks that
/// task so placement (`add_task` / wake `PLACE_LAG`) can still include it in
/// **V**, matching Linux `avg_vruntime()` / `place_entity()`.
///
/// Sleep paths should call [`BaseScheduler::account_sleep`] so lag is saved;
/// the matching wake then goes through [`BaseScheduler::put_prev_task`], which
/// applies Linux-style `PLACE_LAG` placement before requeue.
///
/// If no task is eligible, the one with the smallest deadline is chosen as
/// a fallback to guarantee progress.
///
/// `MAX_TIME_SLICE` is the default request length in timer ticks.
pub struct EevdfScheduler<T, const MAX_TIME_SLICE: usize> {
    /// Ready tasks keyed by `(deadline, id)`.
    ready_queue: BTreeMap<(isize, u64), Arc<EevdfEntity<T, MAX_TIME_SLICE>>>,
    /// Secondary index keyed by `(vruntime, id)` for O(log N) min-vruntime and
    /// O(log N + E) eligible-task range queries.
    vrt_set: BTreeSet<(isize, u64)>,
    /// Reverse map from task id to its current deadline, used to look up the
    /// `ready_queue` key for tasks found via `vrt_set` range queries.
    id_to_deadline: BTreeMap<u64, isize>,
    min_vruntime: isize,
    /// Incrementally maintained for O(1) ready-queue `avg_vruntime` queries.
    total_weighted_vrt: i128,
    total_weight: i128,
    /// Running task that has been dequeued from the ready queue (Linux
    /// `cfs_rq->curr`). Included in placement **V** via [`Self::system_vruntime`].
    curr: Option<Arc<EevdfEntity<T, MAX_TIME_SLICE>>>,
    /// Monotonically increasing counter used as tie-breaker in queue keys.
    /// `u64` ensures wrap-around only after ~1.8×10¹⁹ scheduling events.
    id_pool: u64,
    stats_enabled: bool,
    stats: EevdfStats,
    #[cfg(any(test, unittest))]
    debug_force_no_eligible: bool,
}

impl<T, const S: usize> EevdfScheduler<T, S> {
    pub const fn new() -> Self {
        Self {
            ready_queue: BTreeMap::new(),
            vrt_set: BTreeSet::new(),
            id_to_deadline: BTreeMap::new(),
            min_vruntime: 0,
            total_weighted_vrt: 0,
            total_weight: 0,
            curr: None,
            id_pool: 0,
            stats_enabled: false,
            stats: EevdfStats {
                picks_total: 0,
                preempt_by_deadline: 0,
                fallback_no_eligible: 0,
                slice_expired: 0,
            },
            #[cfg(any(test, unittest))]
            debug_force_no_eligible: false,
        }
    }

    pub fn scheduler_name() -> &'static str {
        "EEVDF"
    }

    pub fn set_stats_enabled(&mut self, enabled: bool) {
        self.stats_enabled = enabled;
    }

    pub fn stats(&self) -> EevdfStats {
        self.stats
    }

    pub fn reset_stats(&mut self) {
        self.stats = EevdfStats::default();
    }

    #[cfg(any(test, unittest))]
    pub(crate) fn set_debug_force_no_eligible(&mut self, enabled: bool) {
        self.debug_force_no_eligible = enabled;
    }

    fn stat_inc(counter: &mut u64, enabled: bool) {
        if enabled {
            *counter = counter.saturating_add(1);
        }
    }

    fn next_id(&mut self) -> u64 {
        let id = self.id_pool;
        self.id_pool = self.id_pool.wrapping_add(1);
        id
    }

    // ---- internal queue helpers (keep both indices + counters in sync) ----

    fn enqueue(&mut self, task: Arc<EevdfEntity<T, S>>) {
        let vr = task.vruntime();
        let id = task.id();
        let dl = task.deadline();
        let w = task.weight() as i128;

        self.ready_queue.insert((dl, id), task);
        self.vrt_set.insert((vr, id));
        self.id_to_deadline.insert(id, dl);
        self.total_weighted_vrt += vr as i128 * w;
        self.total_weight += w;
    }

    fn dequeue_by_key(&mut self, key: (isize, u64)) -> Option<Arc<EevdfEntity<T, S>>> {
        let task = self.ready_queue.remove(&key)?;
        let vr = task.vruntime();
        let id = task.id();
        let w = task.weight() as i128;

        self.vrt_set.remove(&(vr, id));
        self.id_to_deadline.remove(&id);
        self.total_weighted_vrt -= vr as i128 * w;
        self.total_weight -= w;

        if let Some(&(min_vr, _)) = self.vrt_set.iter().next() {
            self.min_vruntime = self.min_vruntime.max(min_vr);
        }
        Some(task)
    }

    // ---- virtual time ----

    /// Ready-queue-only weighted average **V** (excludes [`Self::curr`]).
    /// Used by `pick_next` after the previous running task has been put back
    /// or blocked, when `curr` is already clear.
    fn avg_vruntime(&self) -> isize {
        if self.total_weight <= 0 {
            self.min_vruntime
        } else {
            (self.total_weighted_vrt / self.total_weight) as isize
        }
    }

    /// V that additionally includes a currently-running task which has been
    /// removed from the ready queue.  O(1).
    fn avg_vruntime_with(&self, current: &EevdfEntity<T, S>) -> isize {
        let cw = current.weight() as i128;
        let wsum = self.total_weighted_vrt + current.vruntime() as i128 * cw;
        let wtot = self.total_weight + cw;
        if wtot <= 0 {
            self.min_vruntime
        } else {
            (wsum / wtot) as isize
        }
    }

    /// Placement **V**: ready-queue average plus [`Self::curr`] when set.
    ///
    /// Matches Linux `avg_vruntime()` accounting for `cfs_rq->curr`. When the
    /// ready queue is empty and only `curr` is runnable, **V** is
    /// `curr.vruntime()`.
    fn system_vruntime(&self) -> isize {
        match self.curr.as_deref() {
            Some(curr) if self.total_weight <= 0 => curr.vruntime(),
            Some(curr) => self.avg_vruntime_with(curr),
            None => self.avg_vruntime(),
        }
    }

    /// Weight sum for placement (ready queue plus `curr` when set).
    fn system_weight(&self) -> i128 {
        match self.curr.as_deref() {
            Some(curr) => self.total_weight + curr.weight() as i128,
            None => self.total_weight,
        }
    }

    /// Clamp |vlag| to about one request at the task's weight so long sleeps
    /// cannot accumulate unbounded positive lag (stand-in for Linux lag decay).
    fn clamp_vlag(lag: isize, weight: isize, request_ticks: usize) -> isize {
        let limit = deadline_delta(request_ticks, weight);
        lag.clamp(-limit, limit)
    }

    /// Linux `PLACE_LAG`: place a waking task so its virtual lag is preserved
    /// after it joins the weighted average.
    ///
    /// Uses [`Self::system_vruntime`] / [`Self::system_weight`] so **V** and
    /// **W** include the dequeued running task. Inflates lag by `(W + w) / W`
    /// before `vruntime = V - lag`, matching `kernel/sched/fair.c`
    /// `place_entity()`.
    fn place_waking_vruntime(&self, task: &EevdfEntity<T, S>) -> isize {
        let avg = self.system_vruntime();
        let load = self.system_weight();
        if load <= 0 {
            return self.min_vruntime;
        }

        let weight = task.weight();
        let mut lag = Self::clamp_vlag(task.vlag(), weight, task.request_ticks()) as i128;
        // vl = (W + w) * vl' / W
        lag = lag * (load + weight as i128) / load;
        (avg as i128 - lag) as isize
    }

    /// Earliest-deadline key among tasks with `vruntime ≤ v`, if any.
    fn earliest_eligible_key(&self, v: isize) -> Option<(isize, u64)> {
        if self.ready_queue.is_empty() {
            return None;
        }

        let (&first_key, first_task) = self.ready_queue.iter().next().unwrap();
        if first_task.vruntime() <= v {
            return Some(first_key);
        }

        self.vrt_set
            .range(..=(v, u64::MAX))
            .map(|&(_, id)| (self.id_to_deadline[&id], id))
            .min()
    }

    fn requeue(&mut self, task: Arc<EevdfEntity<T, S>>) {
        let id = self.next_id();
        task.set_id(id);
        self.enqueue(task);
    }

    /// Test helper: insert a task with its current metadata (no placement).
    #[cfg(any(test, unittest))]
    pub(crate) fn inject_ready_for_test(&mut self, task: Arc<EevdfEntity<T, S>>) {
        task.set_needs_place(false);
        self.requeue(task);
    }
}

impl<T, const S: usize> BaseScheduler for EevdfScheduler<T, S> {
    type SchedItem = Arc<EevdfEntity<T, S>>;

    fn init(&mut self) {}

    fn add_task(&mut self, task: Self::SchedItem) {
        // New task: zero lag at placement V (includes curr when running).
        let vr = self.system_vruntime();
        task.set_vlag(0);
        task.set_needs_place(false);
        task.set_vruntime(vr);
        task.set_deadline(vr + deadline_delta(task.request_ticks(), task.weight()));
        task.reset_slice();
        self.requeue(task);
    }

    fn remove_task(&mut self, task: &Self::SchedItem) -> Option<Self::SchedItem> {
        // Snapshot lag against the average that still includes this task.
        let v = self.avg_vruntime();
        let lag = v - task.vruntime();
        let removed = self.dequeue_by_key((task.deadline(), task.id()))?;
        removed.set_vlag(Self::clamp_vlag(
            lag,
            removed.weight(),
            removed.request_ticks(),
        ));
        removed.set_needs_place(true);
        Some(removed)
    }

    fn account_sleep(&mut self, task: &Self::SchedItem) {
        // `task` is the running task leaving without requeue (block). Include it
        // in V so lag matches Linux `update_entity_lag` for curr.
        let v = self.avg_vruntime_with(task);
        let lag = v - task.vruntime();
        task.set_vlag(Self::clamp_vlag(lag, task.weight(), task.request_ticks()));
        task.set_needs_place(true);
        if self.curr.as_ref().is_some_and(|c| Arc::ptr_eq(c, task)) {
            self.curr = None;
        }
    }

    fn pick_next_task(&mut self) -> Option<Self::SchedItem> {
        if self.ready_queue.is_empty() {
            return None;
        }

        // After put_prev / account_sleep, `curr` is clear; eligible uses ready-only V.
        let v = self.avg_vruntime();
        let mut used_fallback = false;

        let first_key = *self.ready_queue.keys().next().unwrap();

        let eligible_key = self.earliest_eligible_key(v);
        #[cfg(any(test, unittest))]
        let eligible_key = if self.debug_force_no_eligible {
            None
        } else {
            eligible_key
        };

        let key = match eligible_key {
            Some(k) => k,
            None => {
                used_fallback = true;
                first_key
            }
        };

        Self::stat_inc(&mut self.stats.picks_total, self.stats_enabled);
        if used_fallback {
            Self::stat_inc(&mut self.stats.fallback_no_eligible, self.stats_enabled);
        }

        let picked = self.dequeue_by_key(key)?;
        self.curr = Some(picked.clone());
        Some(picked)
    }

    fn put_prev_task(&mut self, prev: Self::SchedItem, preempt: bool) {
        if prev.needs_place() {
            // Wake / post-dequeue re-entry: place by saved vlag, start a new request.
            // Do not clear `curr` — the running task on this rq is still someone else.
            let vr = self.place_waking_vruntime(&prev);
            prev.set_vruntime(vr);
            prev.reset_slice();
            prev.set_deadline(vr + deadline_delta(prev.request_ticks(), prev.weight()));
            prev.set_needs_place(false);
            self.requeue(prev);
            return;
        }

        if self.curr.as_ref().is_some_and(|c| Arc::ptr_eq(c, &prev)) {
            self.curr = None;
        }

        let vr = prev.vruntime().max(self.min_vruntime);
        prev.set_vruntime(vr);

        if preempt && prev.slice() > 0 {
            // Task was preempted before its slice expired.
            if prev.deadline() <= vr {
                // Deadline passed while the task was off-CPU (e.g. min_vruntime
                // advanced).  Assign a new deadline proportional to the
                // *remaining* slice, not the full request — using the full
                // request would over-reward a task that already consumed part
                // of its request.
                prev.set_deadline(vr + deadline_delta(prev.slice() as usize, prev.weight()));
            }
            // else: deadline still valid, preserve it so the task keeps its
            // place in the deadline-ordered queue.
        } else {
            prev.reset_slice();
            prev.set_deadline(vr + deadline_delta(prev.request_ticks(), prev.weight()));
        }

        self.requeue(prev);
    }

    fn task_tick(&mut self, current: &Self::SchedItem) -> bool {
        let delta = vruntime_delta(current.weight());
        current.vruntime.fetch_add(delta, Ordering::Release);

        let old_slice = current.slice.fetch_sub(1, Ordering::Release);
        if old_slice <= 1 {
            Self::stat_inc(&mut self.stats.slice_expired, self.stats_enabled);
            return true;
        }

        // Same rule as pick_next: preempt for the earliest eligible deadline,
        // not only when the global min-deadline task happens to be eligible.
        let v = self.avg_vruntime_with(current);
        if let Some((dl, _)) = self.earliest_eligible_key(v)
            && dl < current.deadline()
        {
            Self::stat_inc(&mut self.stats.preempt_by_deadline, self.stats_enabled);
            return true;
        }

        false
    }

    fn set_priority(&mut self, task: &Self::SchedItem, prio: isize) -> bool {
        if !(-20..=19).contains(&prio) {
            return false;
        }

        let old_weight = task.weight();
        if let Some(removed) = self.dequeue_by_key((task.deadline(), task.id())) {
            removed.nice.store(prio, Ordering::Release);
            let new_weight = removed.weight();
            // Keep virtual lag consistent with weight-scaled lag when nice changes.
            if new_weight != old_weight && new_weight != 0 {
                let scaled = removed.vlag() as i128 * old_weight as i128 / new_weight as i128;
                removed.set_vlag(scaled as isize);
            }
            let vr = removed.vruntime();
            let rem = removed.slice().max(1) as usize;
            removed.set_deadline(vr + deadline_delta(rem, new_weight));
            self.requeue(removed);
        } else {
            task.nice.store(prio, Ordering::Release);
            let new_weight = task.weight();
            if new_weight != old_weight && new_weight != 0 {
                let scaled = task.vlag() as i128 * old_weight as i128 / new_weight as i128;
                task.set_vlag(scaled as isize);
            }
            let rem = task.slice().max(1) as usize;
            task.set_deadline(task.vruntime() + deadline_delta(rem, new_weight));
        }

        true
    }
}

impl<T, const S: usize> Default for EevdfScheduler<T, S> {
    fn default() -> Self {
        Self::new()
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
use core::{
    ops::Deref,
    sync::atomic::{AtomicIsize, AtomicU64, Ordering},
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

/// Deadline increment for a full slice: `ticks × NICE_0_WEIGHT² / weight`.
fn deadline_delta(ticks: usize, weight: isize) -> isize {
    (ticks as i128 * NICE_0_WEIGHT * NICE_0_WEIGHT / weight as i128) as isize
}

/// Per-task EEVDF scheduling entity.
///
/// Wraps an inner value `T` with scheduling metadata: virtual runtime,
/// virtual deadline, nice value, remaining time-slice, and a monotonic id
/// used as tie-breaker in the deadline-ordered ready queue.
pub struct EevdfEntity<T, const MAX_TIME_SLICE: usize> {
    inner: T,
    vruntime: AtomicIsize,
    deadline: AtomicIsize,
    nice: AtomicIsize,
    slice: AtomicIsize,
    id: AtomicU64,
}

impl<T, const S: usize> EevdfEntity<T, S> {
    pub const fn new(inner: T) -> Self {
        Self {
            inner,
            vruntime: AtomicIsize::new(0),
            deadline: AtomicIsize::new(0),
            nice: AtomicIsize::new(0),
            slice: AtomicIsize::new(S as isize),
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

    /// Accessors only used in unit tests to construct or inspect edge-case scenarios.
    #[cfg(any(test, unittest))]
    pub(crate) fn set_deadline_for_test(&self, d: isize) {
        self.set_deadline(d);
    }

    #[cfg(any(test, unittest))]
    pub(crate) fn vruntime_for_test(&self) -> isize {
        self.vruntime()
    }

    #[cfg(any(test, unittest))]
    pub(crate) fn slice_for_test(&self) -> isize {
        self.slice()
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

    pub(crate) fn reset_slice(&self) {
        self.slice.store(S as isize, Ordering::Release);
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
/// Each task carries its own virtual runtime (`vruntime`) and virtual
/// deadline (`deadline`).  At every scheduling decision the scheduler
/// computes the system virtual time **V** (the load-weighted average
/// vruntime of all runnable tasks) and picks the task with the smallest
/// deadline among those whose `vruntime ≤ V` (i.e. *eligible* tasks that
/// have not consumed more than their fair share).
///
/// If no task is eligible, the one with the smallest deadline is chosen as
/// a fallback to guarantee progress.
///
/// `MAX_TIME_SLICE` is the base time-slice length in timer ticks.
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
    /// Incrementally maintained for O(1) `avg_vruntime` queries.
    total_weighted_vrt: i128,
    total_weight: i128,
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

    /// System virtual time **V**: load-weighted average vruntime of all
    /// tasks in the ready queue.  O(1) via incremental counters.
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
}

impl<T, const S: usize> BaseScheduler for EevdfScheduler<T, S> {
    type SchedItem = Arc<EevdfEntity<T, S>>;

    fn init(&mut self) {}

    fn add_task(&mut self, task: Self::SchedItem) {
        let vr = task.vruntime().max(self.min_vruntime);
        task.set_vruntime(vr);
        task.set_deadline(vr + deadline_delta(S, task.weight()));
        task.reset_slice();

        let id = self.next_id();
        task.set_id(id);
        self.enqueue(task);
    }

    fn remove_task(&mut self, task: &Self::SchedItem) -> Option<Self::SchedItem> {
        self.dequeue_by_key((task.deadline(), task.id()))
    }

    fn pick_next_task(&mut self) -> Option<Self::SchedItem> {
        if self.ready_queue.is_empty() {
            return None;
        }

        let v = self.avg_vruntime();
        let mut used_fallback = false;

        // Fast path: if the min-deadline task is eligible, return it immediately — O(1).
        let first_key = *self.ready_queue.keys().next().unwrap();
        let first_vruntime = self.ready_queue[&first_key].vruntime();

        let first_eligible = first_vruntime <= v;
        // In tests, allow forcing the "no eligible" path to exercise fallback stats.
        #[cfg(any(test, unittest))]
        let first_eligible = first_eligible && !self.debug_force_no_eligible;

        let key = if first_eligible {
            first_key
        } else {
            // Slow path: min-deadline task is not eligible.
            // Range-query vrt_set for all eligible tasks (vruntime ≤ V) and find
            // the one with the earliest deadline — O(log N + E).
            let eligible_key = self
                .vrt_set
                .range(..=(v, u64::MAX))
                .map(|&(_, id)| (self.id_to_deadline[&id], id))
                .min();

            #[cfg(any(test, unittest))]
            let eligible_key: Option<(isize, u64)> = if self.debug_force_no_eligible {
                None
            } else {
                eligible_key
            };

            match eligible_key {
                Some(k) => k,
                None => {
                    // Fallback: no eligible task — pick min deadline unconditionally.
                    used_fallback = true;
                    first_key
                }
            }
        };

        Self::stat_inc(&mut self.stats.picks_total, self.stats_enabled);
        if used_fallback {
            Self::stat_inc(&mut self.stats.fallback_no_eligible, self.stats_enabled);
        }

        self.dequeue_by_key(key)
    }

    fn put_prev_task(&mut self, prev: Self::SchedItem, preempt: bool) {
        let vr = prev.vruntime().max(self.min_vruntime);
        prev.set_vruntime(vr);

        if preempt && prev.slice() > 0 {
            // Task was preempted before its slice expired.
            if prev.deadline() <= vr {
                // Deadline passed while the task was off-CPU (e.g. min_vruntime
                // advanced).  Assign a new deadline proportional to the
                // *remaining* slice, not the full slice — using the full slice
                // would over-reward a task that already consumed part of its
                // request.
                prev.set_deadline(vr + deadline_delta(prev.slice() as usize, prev.weight()));
            }
            // else: deadline still valid, preserve it so the task keeps its
            // place in the deadline-ordered queue.
        } else {
            prev.reset_slice();
            prev.set_deadline(vr + deadline_delta(S, prev.weight()));
        }

        let id = self.next_id();
        prev.set_id(id);
        self.enqueue(prev);
    }

    fn task_tick(&mut self, current: &Self::SchedItem) -> bool {
        let delta = vruntime_delta(current.weight());
        current.vruntime.fetch_add(delta, Ordering::Release);

        let old_slice = current.slice.fetch_sub(1, Ordering::Release);
        if old_slice <= 1 {
            Self::stat_inc(&mut self.stats.slice_expired, self.stats_enabled);
            return true;
        }

        // Deadline-driven preemption: if the earliest-deadline ready task is
        // eligible and has a tighter deadline than the running task, preempt.
        if let Some((_, head)) = self.ready_queue.iter().next() {
            let v = self.avg_vruntime_with(current);
            if head.vruntime() <= v && head.deadline() < current.deadline() {
                Self::stat_inc(&mut self.stats.preempt_by_deadline, self.stats_enabled);
                return true;
            }
        }

        false
    }

    fn set_priority(&mut self, task: &Self::SchedItem, prio: isize) -> bool {
        if !(-20..=19).contains(&prio) {
            return false;
        }

        if let Some(removed) = self.dequeue_by_key((task.deadline(), task.id())) {
            removed.nice.store(prio, Ordering::Release);
            let vr = removed.vruntime();
            removed.set_deadline(vr + deadline_delta(S, removed.weight()));
            let id = self.next_id();
            removed.set_id(id);
            self.enqueue(removed);
        } else {
            task.nice.store(prio, Ordering::Release);
            task.set_deadline(task.vruntime() + deadline_delta(S, task.weight()));
        }

        true
    }
}

impl<T, const S: usize> Default for EevdfScheduler<T, S> {
    fn default() -> Self {
        Self::new()
    }
}

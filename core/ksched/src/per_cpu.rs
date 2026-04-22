// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-CPU heterogeneous scheduler with separated scheduling metadata.
//!
//! ## Design
//!
//! Traditional scheduler designs embed scheduling metadata (vruntime, deadline,
//! time-slice, …) directly inside the task struct via a scheduler-specific
//! wrapper type (e.g. `EevdfEntity`).  This couples the task representation to
//! the scheduler algorithm and makes it impossible for different CPUs to run
//! different algorithms on the same task without type-unsafe conversions.
//!
//! This module takes a different approach: **scheduling metadata lives inside
//! the scheduler, not inside the task**.  Each scheduler variant maintains its
//! own metadata table keyed by a unique task ID.  The task type `T` only needs
//! to implement [`HasSchedulerId`] — a single method that returns a stable u64
//! identifier.
//!
//! ```text
//! ┌─────────────────────────────────────────┐
//! │  PerCpuScheduler<T>                     │
//! │                                         │
//! │  ready_queue: BTreeMap<Key, Arc<T>>     │  ← tasks (no sched fields)
//! │  metadata:    BTreeMap<u64, Meta>       │  ← all sched state
//! └─────────────────────────────────────────┘
//! ```
//!
//! Benefits:
//! * Tasks can migrate between CPUs with different algorithms without
//!   any type conversion — the receiving scheduler simply creates a new
//!   metadata entry.
//! * No wasted memory from unused fields (e.g. an EEVDF vruntime field
//!   sitting inside an RR task).
//!
//! Supported algorithms: **EEVDF**, **FIFO**, **Round-Robin**.

use alloc::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::Arc,
};

use crate::{BaseScheduler, eevdf::EevdfStats};

// ═══════════════════════════════════════════════════════════════════════════
// Public trait
// ═══════════════════════════════════════════════════════════════════════════

/// Tasks that can be scheduled by [`PerCpuScheduler`] must implement this
/// trait to expose a stable unique identifier.
pub trait HasSchedulerId: Send + Sync {
    fn sched_id(&self) -> u64;
}

// ═══════════════════════════════════════════════════════════════════════════
// Scheduler kind
// ═══════════════════════════════════════════════════════════════════════════

/// Which scheduling algorithm a CPU should use.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SchedulerKind {
    #[default]
    Eevdf = 0,
    Fifo  = 1,
    Rr    = 2,
}

impl SchedulerKind {
    pub fn name(self) -> &'static str {
        match self {
            SchedulerKind::Eevdf => "EEVDF",
            SchedulerKind::Fifo => "FIFO",
            SchedulerKind::Rr => "Round-Robin",
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Shared weight / timing helpers  (same as in eevdf.rs)
// ═══════════════════════════════════════════════════════════════════════════

const NICE_0_WEIGHT: i128 = 1024;
const DEFAULT_TIME_SLICE: isize = 5;

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

fn vruntime_delta(weight: isize) -> isize {
    (NICE_0_WEIGHT * NICE_0_WEIGHT / weight as i128) as isize
}

fn deadline_delta(ticks: isize, weight: isize) -> isize {
    (ticks as i128 * NICE_0_WEIGHT * NICE_0_WEIGHT / weight as i128) as isize
}

// ═══════════════════════════════════════════════════════════════════════════
// EEVDF per-task metadata
// ═══════════════════════════════════════════════════════════════════════════

#[derive(Clone)]
struct EevdfMeta {
    vruntime: isize,
    deadline: isize,
    nice: isize,
    slice: isize, // remaining time-slice ticks
}

impl EevdfMeta {
    fn weight(&self) -> isize {
        nice_to_weight(self.nice)
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// EEVDF scheduler (metadata-separated)
// ═══════════════════════════════════════════════════════════════════════════

pub struct MetadataEevdf<T: HasSchedulerId> {
    /// Tasks in the ready queue, keyed by (deadline, task_id).
    ready_queue: BTreeMap<(isize, u64), Arc<T>>,
    /// Secondary index by (vruntime, task_id) for eligible-task range queries.
    vrt_set: BTreeSet<(isize, u64)>,
    /// Scheduling metadata for every known task (queued *and* currently running).
    metadata: BTreeMap<u64, EevdfMeta>,
    /// Incremental counters for O(1) avg_vruntime over the *ready* queue.
    total_weighted_vrt: i128,
    total_weight: i128,
    min_vruntime: isize,
    stats_enabled: bool,
    stats: EevdfStats,
}

impl<T: HasSchedulerId> MetadataEevdf<T> {
    fn new() -> Self {
        Self {
            ready_queue: BTreeMap::new(),
            vrt_set: BTreeSet::new(),
            metadata: BTreeMap::new(),
            total_weighted_vrt: 0,
            total_weight: 0,
            min_vruntime: 0,
            stats_enabled: false,
            stats: EevdfStats::default(),
        }
    }

    #[inline]
    fn stat_inc(counter: &mut u64, enabled: bool) {
        if enabled {
            *counter = counter.saturating_add(1);
        }
    }

    /// System virtual time V — load-weighted average vruntime of queued tasks.
    fn avg_vruntime(&self) -> isize {
        if self.total_weight <= 0 {
            self.min_vruntime
        } else {
            (self.total_weighted_vrt / self.total_weight) as isize
        }
    }

    /// V that also accounts for a running task (not in the ready queue).
    fn avg_vruntime_with(&self, meta: &EevdfMeta) -> isize {
        let cw = meta.weight() as i128;
        let wsum = self.total_weighted_vrt + meta.vruntime as i128 * cw;
        let wtot = self.total_weight + cw;
        if wtot <= 0 {
            self.min_vruntime
        } else {
            (wsum / wtot) as isize
        }
    }

    fn enqueue(&mut self, id: u64, task: Arc<T>, meta: &EevdfMeta) {
        let w = meta.weight() as i128;
        let vr = meta.vruntime;
        let dl = meta.deadline;
        self.ready_queue.insert((dl, id), task);
        self.vrt_set.insert((vr, id));
        self.total_weighted_vrt += vr as i128 * w;
        self.total_weight += w;
    }

    fn dequeue(&mut self, dl: isize, id: u64) -> Option<Arc<T>> {
        let task = self.ready_queue.remove(&(dl, id))?;
        let meta = &self.metadata[&id];
        let vr = meta.vruntime;
        let w = meta.weight() as i128;
        self.vrt_set.remove(&(vr, id));
        self.total_weighted_vrt -= vr as i128 * w;
        self.total_weight -= w;
        if let Some(&(min_vr, _)) = self.vrt_set.iter().next() {
            self.min_vruntime = self.min_vruntime.max(min_vr);
        }
        Some(task)
    }
}

impl<T: HasSchedulerId> BaseScheduler for MetadataEevdf<T> {
    type SchedItem = Arc<T>;

    fn init(&mut self) {}

    fn add_task(&mut self, task: Self::SchedItem) {
        let id = task.sched_id();
        let vr = self.min_vruntime;
        let meta = EevdfMeta {
            vruntime: vr,
            deadline: vr + deadline_delta(DEFAULT_TIME_SLICE, nice_to_weight(0)),
            nice: 0,
            slice: DEFAULT_TIME_SLICE,
        };
        self.enqueue(id, task, &meta);
        self.metadata.insert(id, meta);
    }

    fn remove_task(&mut self, task: &Self::SchedItem) -> Option<Self::SchedItem> {
        let id = task.sched_id();
        let dl = self.metadata.get(&id)?.deadline;
        let removed = self.dequeue(dl, id)?;
        self.metadata.remove(&id);
        Some(removed)
    }

    fn pick_next_task(&mut self) -> Option<Self::SchedItem> {
        if self.ready_queue.is_empty() {
            return None;
        }
        let v = self.avg_vruntime();
        let mut used_fallback = false;

        // Fast path: min-deadline task is eligible.
        let &(first_dl, first_id) = self.ready_queue.keys().next().unwrap();
        let first_vr = self.metadata[&first_id].vruntime;
        let key = if first_vr <= v {
            (first_dl, first_id)
        } else {
            // Slow path: find min-deadline among eligible tasks.
            let eligible = self
                .vrt_set
                .range(..=(v, u64::MAX))
                .map(|&(_, id)| (self.metadata[&id].deadline, id))
                .min();
            match eligible {
                Some(k) => k,
                None => {
                    // fallback
                    used_fallback = true;
                    (first_dl, first_id)
                }
            }
        };
        Self::stat_inc(&mut self.stats.picks_total, self.stats_enabled);
        if used_fallback {
            Self::stat_inc(&mut self.stats.fallback_no_eligible, self.stats_enabled);
        }
        self.dequeue(key.0, key.1)
        // Note: metadata entry for this task is intentionally kept alive so
        // that task_tick / put_prev_task can access it while the task runs.
    }

    fn put_prev_task(&mut self, prev: Self::SchedItem, preempt: bool) {
        let id = prev.sched_id();
        let Some(meta) = self.metadata.get_mut(&id) else {
            // prev is not tracked (e.g. idle task) — discard without re-queuing
            return;
        };
        let vr = meta.vruntime.max(self.min_vruntime);
        meta.vruntime = vr;

        if preempt && meta.slice > 0 {
            // Keep deadline if still valid; otherwise assign one based on
            // remaining slice (avoid over-rewarding partially-run tasks).
            if meta.deadline <= vr {
                let remaining = meta.slice;
                meta.deadline = vr + deadline_delta(remaining, meta.weight());
            }
        } else {
            meta.slice = DEFAULT_TIME_SLICE;
            meta.deadline = vr + deadline_delta(DEFAULT_TIME_SLICE, meta.weight());
        }
        let snapshot = meta.clone();
        self.enqueue(id, prev, &snapshot);
        // Update the stored metadata to reflect new deadline/vruntime.
        *self.metadata.get_mut(&id).unwrap() = snapshot;
    }

    fn task_tick(&mut self, current: &Self::SchedItem) -> bool {
        let id = current.sched_id();
        let Some(meta) = self.metadata.get_mut(&id) else {
            // current is not tracked (e.g. idle task) — nothing to do
            return false;
        };
        meta.vruntime += vruntime_delta(meta.weight());
        meta.slice -= 1;

        if meta.slice <= 0 {
            Self::stat_inc(&mut self.stats.slice_expired, self.stats_enabled);
            return true; // slice expired
        }

        // Deadline-driven preemption: is there an eligible ready task with a
        // tighter deadline than the running task?
        let snapshot = meta.clone();
        let v = self.avg_vruntime_with(&snapshot);
        if let Some((&(head_dl, head_id), _)) = self.ready_queue.iter().next() {
            let head_vr = self.metadata[&head_id].vruntime;
            if head_vr <= v && head_dl < snapshot.deadline {
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
        let id = task.sched_id();
        let Some(meta) = self.metadata.get_mut(&id) else {
            return false;
        };

        // If task is currently queued, re-insert with updated deadline.
        let old_dl = meta.deadline;
        meta.nice = prio;
        meta.deadline = meta.vruntime + deadline_delta(DEFAULT_TIME_SLICE, meta.weight());
        let new_dl = meta.deadline;

        if let Some(task_arc) = self.ready_queue.remove(&(old_dl, id)) {
            let snapshot = self.metadata[&id].clone();
            let vr = snapshot.vruntime;
            let w = snapshot.weight() as i128;
            // Patch totals: weight changed, vruntime stayed the same.
            self.total_weighted_vrt = self.total_weighted_vrt
                - vr as i128 * nice_to_weight(prio) as i128
                + vr as i128 * w;
            self.vrt_set.remove(&(vr, id));
            self.ready_queue.insert((new_dl, id), task_arc);
            self.vrt_set.insert((vr, id));
        }
        true
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// FIFO (no metadata needed)
// ═══════════════════════════════════════════════════════════════════════════

pub struct MetadataFifo<T> {
    queue: VecDeque<Arc<T>>,
}

impl<T> MetadataFifo<T> {
    fn new() -> Self {
        Self {
            queue: VecDeque::new(),
        }
    }
}

impl<T: HasSchedulerId> BaseScheduler for MetadataFifo<T> {
    type SchedItem = Arc<T>;

    fn init(&mut self) {}

    fn add_task(&mut self, task: Self::SchedItem) {
        self.queue.push_back(task);
    }

    fn remove_task(&mut self, task: &Self::SchedItem) -> Option<Self::SchedItem> {
        let id = task.sched_id();
        let pos = self.queue.iter().position(|t| t.sched_id() == id)?;
        self.queue.remove(pos)
    }

    fn pick_next_task(&mut self) -> Option<Self::SchedItem> {
        self.queue.pop_front()
    }

    fn put_prev_task(&mut self, prev: Self::SchedItem, _preempt: bool) {
        self.queue.push_back(prev);
    }

    fn task_tick(&mut self, _current: &Self::SchedItem) -> bool {
        false
    }

    fn set_priority(&mut self, _task: &Self::SchedItem, _prio: isize) -> bool {
        false
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Round-Robin (slice metadata in scheduler)
// ═══════════════════════════════════════════════════════════════════════════

pub struct MetadataRr<T: HasSchedulerId> {
    queue: VecDeque<Arc<T>>,
    slices: BTreeMap<u64, isize>, // task_id -> remaining slice
}

impl<T: HasSchedulerId> MetadataRr<T> {
    fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            slices: BTreeMap::new(),
        }
    }
}

impl<T: HasSchedulerId> BaseScheduler for MetadataRr<T> {
    type SchedItem = Arc<T>;

    fn init(&mut self) {}

    fn add_task(&mut self, task: Self::SchedItem) {
        let id = task.sched_id();
        self.slices.insert(id, DEFAULT_TIME_SLICE);
        self.queue.push_back(task);
    }

    fn remove_task(&mut self, task: &Self::SchedItem) -> Option<Self::SchedItem> {
        let id = task.sched_id();
        let pos = self.queue.iter().position(|t| t.sched_id() == id)?;
        self.slices.remove(&id);
        self.queue.remove(pos)
    }

    fn pick_next_task(&mut self) -> Option<Self::SchedItem> {
        self.queue.pop_front()
    }

    fn put_prev_task(&mut self, prev: Self::SchedItem, preempt: bool) {
        let id = prev.sched_id();
        let slice = self.slices.entry(id).or_insert(DEFAULT_TIME_SLICE);
        if preempt && *slice > 0 {
            self.queue.push_front(prev);
        } else {
            *slice = DEFAULT_TIME_SLICE;
            self.queue.push_back(prev);
        }
    }

    fn task_tick(&mut self, current: &Self::SchedItem) -> bool {
        let id = current.sched_id();
        let slice = self.slices.entry(id).or_insert(DEFAULT_TIME_SLICE);
        *slice -= 1;
        *slice <= 0
    }

    fn set_priority(&mut self, _task: &Self::SchedItem, _prio: isize) -> bool {
        false
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Per-CPU dispatcher
// ═══════════════════════════════════════════════════════════════════════════

/// A per-CPU scheduler that dispatches to one of the supported algorithms.
///
/// All variants share `Arc<T>` as [`BaseScheduler::SchedItem`], where `T`
/// implements [`HasSchedulerId`].  Scheduling metadata (vruntime, deadline,
/// time-slice, …) lives inside the scheduler, **not** inside the task struct.
/// This means tasks can migrate between CPUs running different algorithms
/// without any type conversion.
pub enum PerCpuScheduler<T: HasSchedulerId> {
    Eevdf(MetadataEevdf<T>),
    Fifo(MetadataFifo<T>),
    Rr(MetadataRr<T>),
}

impl<T: HasSchedulerId> PerCpuScheduler<T> {
    pub fn new(kind: SchedulerKind) -> Self {
        match kind {
            SchedulerKind::Eevdf => Self::Eevdf(MetadataEevdf::new()),
            SchedulerKind::Fifo => Self::Fifo(MetadataFifo::new()),
            SchedulerKind::Rr => Self::Rr(MetadataRr::new()),
        }
    }

    pub fn scheduler_name() -> &'static str {
        "per-cpu (EEVDF / FIFO / RR)"
    }

    pub fn instance_name(&self) -> &'static str {
        match self {
            Self::Eevdf(_) => "EEVDF",
            Self::Fifo(_) => "FIFO",
            Self::Rr(_) => "Round-Robin",
        }
    }

    // EEVDF stats passthrough (no-op/default for FIFO/RR).
    pub fn set_stats_enabled(&mut self, enabled: bool) {
        if let Self::Eevdf(s) = self {
            s.stats_enabled = enabled;
        }
    }

    pub fn stats(&self) -> EevdfStats {
        match self {
            Self::Eevdf(s) => s.stats,
            Self::Fifo(_) | Self::Rr(_) => EevdfStats::default(),
        }
    }

    pub fn reset_stats(&mut self) {
        if let Self::Eevdf(s) = self {
            s.stats = EevdfStats::default();
        }
    }
}

impl<T: HasSchedulerId> BaseScheduler for PerCpuScheduler<T> {
    type SchedItem = Arc<T>;

    fn init(&mut self) {
        match self {
            Self::Eevdf(s) => s.init(),
            Self::Fifo(s) => s.init(),
            Self::Rr(s) => s.init(),
        }
    }

    fn add_task(&mut self, task: Self::SchedItem) {
        match self {
            Self::Eevdf(s) => s.add_task(task),
            Self::Fifo(s) => s.add_task(task),
            Self::Rr(s) => s.add_task(task),
        }
    }

    fn remove_task(&mut self, task: &Self::SchedItem) -> Option<Self::SchedItem> {
        match self {
            Self::Eevdf(s) => s.remove_task(task),
            Self::Fifo(s) => s.remove_task(task),
            Self::Rr(s) => s.remove_task(task),
        }
    }

    fn pick_next_task(&mut self) -> Option<Self::SchedItem> {
        match self {
            Self::Eevdf(s) => s.pick_next_task(),
            Self::Fifo(s) => s.pick_next_task(),
            Self::Rr(s) => s.pick_next_task(),
        }
    }

    fn put_prev_task(&mut self, prev: Self::SchedItem, preempt: bool) {
        match self {
            Self::Eevdf(s) => s.put_prev_task(prev, preempt),
            Self::Fifo(s) => s.put_prev_task(prev, preempt),
            Self::Rr(s) => s.put_prev_task(prev, preempt),
        }
    }

    fn task_tick(&mut self, current: &Self::SchedItem) -> bool {
        match self {
            Self::Eevdf(s) => s.task_tick(current),
            Self::Fifo(s) => s.task_tick(current),
            Self::Rr(s) => s.task_tick(current),
        }
    }

    fn set_priority(&mut self, task: &Self::SchedItem, prio: isize) -> bool {
        match self {
            Self::Eevdf(s) => s.set_priority(task, prio),
            Self::Fifo(s) => s.set_priority(task, prio),
            Self::Rr(s) => s.set_priority(task, prio),
        }
    }
}

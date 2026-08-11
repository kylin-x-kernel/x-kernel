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

use crate::{BaseScheduler, CurrentDisposition};

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

/// Load-weighted average vruntime: `(Σw·v + w_c·v_c) / (Σw + w_c)`.
///
/// Shared by ready-queue+running-entity **V** paths so Arc entities and
/// [`RunningEntitySnapshot`] cannot drift apart. `empty_fallback` is used when
/// the combined weight is non-positive (typically [`EevdfScheduler::min_vruntime`]).
#[inline]
fn weighted_avg_vruntime(
    queue_weighted_vrt: i128,
    queue_weight: i128,
    curr_vruntime: isize,
    curr_weight: isize,
    empty_fallback: isize,
) -> isize {
    let cw = curr_weight as i128;
    let wsum = queue_weighted_vrt + curr_vruntime as i128 * cw;
    let wtot = queue_weight + cw;
    if wtot <= 0 {
        empty_fallback
    } else {
        (wsum / wtot) as isize
    }
}

/// Linux `entity_before` on raw `(deadline, id)` keys.
#[inline]
fn deadline_before(a_deadline: isize, a_id: u64, b_deadline: isize, b_id: u64) -> bool {
    a_deadline < b_deadline || (a_deadline == b_deadline && a_id < b_id)
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
    /// When set, the next [`BaseScheduler::enqueue_task`] applies lag-based
    /// placement (wake / migrate-in after deactivate), not yield/preempt requeue.
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
    /// Times `pick_next` preferred a one-shot wake buddy over EEVDF order.
    pub wake_handoff: u64,
    /// Times a new wake buddy was ignored because an earlier-deadline buddy
    /// was already nominated.
    pub wake_handoff_skipped_busy: u64,
    /// Involuntary probes that sync-preempted for an eligible next buddy.
    pub wake_sync_preempt: u64,
}

/// Non-owning cache of the dequeued running entity (Linux `cfs_rq->curr`).
///
/// Stores only values needed for placement **V** and peer-preempt probes so the
/// scheduler never pins task lifetime through `curr`.
///
/// # Snapshot freshness
///
/// Any mutation of a running entity's scheduling fields (`vruntime`, `deadline`,
/// `weight` / nice) must refresh this snapshot before the next
/// [`EevdfScheduler::system_vruntime`] / [`EevdfScheduler::peer_preempts_curr`]
/// use. Production paths do this in [`BaseScheduler::task_tick`] and
/// [`BaseScheduler::set_priority`]; test-only mutators must call
/// `refresh_curr_snapshot_for_test`.
#[derive(Clone, Copy, Debug)]
struct RunningEntitySnapshot {
    id: u64,
    vruntime: isize,
    deadline: isize,
    weight: isize,
}

/// Per-task EEVDF (Earliest Eligible Virtual Deadline First) scheduler.
///
/// Each task carries virtual runtime (`vruntime`), virtual deadline, and
/// virtual lag (`vlag`). At every scheduling decision the scheduler computes
/// the system virtual time **V** (load-weighted average vruntime of runnable
/// tasks) and picks the task with the smallest deadline among those with
/// `vruntime ≤ V` (eligible / non-negative lag).
///
/// The running task is dequeued from the ready tree. [`Self::curr`] caches only
/// scheduling values for that task (Linux `cfs_rq->curr` accounting), never an
/// owning `Arc`. Placement (`PLACE_LAG`) and involuntary picks include `curr` in
/// **V** / deadline comparison the same way Linux `avg_vruntime()` /
/// `pick_eevdf()` do: **do not** put `curr` back onto the ready queue before
/// deciding whether a wakee should preempt it.
///
/// Running tasks must leave through [`BaseScheduler::leave_current`]. Block and
/// Migrate snapshot lag; the matching wake / migrate-in goes through
/// [`BaseScheduler::enqueue_task`], which applies Linux-style `PLACE_LAG` with a
/// **full request** deadline (`vd = ve + r/w`), not a synthetic one-tick hint.
///
/// If no task is eligible, the one with the smallest deadline is chosen as
/// a fallback to guarantee progress.
///
/// ## Wake latency
///
/// 1. **Eligibility** — wake placement clamps `vruntime` to at most system **V**
///    so a wakee is not ignored while eligible peers run.
/// 2. **Pick vs `curr`** — involuntary preemption probes with `curr` off-tree
///    (`peer_preempts_curr`); only an earlier-deadline eligible wakee forces
///    `leave_current(Preempt)`+`pick`, unless a sync wake armed an eligible next buddy.
/// 3. **Next buddy** — like Linux `NEXT_BUDDY` / `set_preempt_buddy`, a wake
///    onto a busy rq nominates the wakee as a one-shot pick hint when the waker
///    blocks. Existing buddies with an earlier deadline are kept so concurrent
///    wakes cannot leapfrog. Buddy pick still requires eligibility.
/// 4. **WF_SYNC** — when the waker marks a sync wake (e.g. futex), an eligible
///    next buddy may preempt `curr` even with a later deadline, so the wakee
///    need not wait for the waker to block. Hint only: ineligible buddies never
///    force preemption.
/// 5. **Tick** — [`Self::task_tick`] reschedules on deadline expiry only when
///    another eligible task is waiting, so a lone task is not thrashed every tick.
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
    /// Non-owning snapshot of the dequeued running task (Linux `cfs_rq->curr`).
    /// Included in placement **V** via [`Self::system_vruntime`]. Must stay in
    /// sync with the live running entity (see [`RunningEntitySnapshot`]).
    curr: Option<RunningEntitySnapshot>,
    /// One-shot preferred wakee for the next pick (same run queue only).
    next_buddy: Option<Arc<EevdfEntity<T, MAX_TIME_SLICE>>>,
    /// Armed by a `WF_SYNC` wake: eligible [`Self::next_buddy`] may preempt
    /// `curr` even when its deadline is later (waker expects to sleep soon).
    sync_preempt_pending: bool,
    /// One-shot: after a sync preempt, the next pick may honour an eligible
    /// buddy even when another ready task has an earlier deadline (the
    /// just-preempted `curr` that was put back). Cleared by
    /// [`Self::try_pick_wake_buddy`].
    prefer_sync_buddy: bool,
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
            next_buddy: None,
            sync_preempt_pending: false,
            prefer_sync_buddy: false,
            id_pool: 0,
            stats_enabled: false,
            stats: EevdfStats {
                picks_total: 0,
                preempt_by_deadline: 0,
                fallback_no_eligible: 0,
                slice_expired: 0,
                wake_handoff: 0,
                wake_handoff_skipped_busy: 0,
                wake_sync_preempt: 0,
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

    fn clear_next_buddy_if(&mut self, task: &Arc<EevdfEntity<T, S>>) {
        if self
            .next_buddy
            .as_ref()
            .is_some_and(|buddy| Arc::ptr_eq(buddy, task))
        {
            self.next_buddy = None;
        }
    }

    fn snapshot_of(task: &EevdfEntity<T, S>) -> RunningEntitySnapshot {
        RunningEntitySnapshot {
            id: task.id(),
            vruntime: task.vruntime(),
            deadline: task.deadline(),
            weight: task.weight(),
        }
    }

    fn set_curr_from(&mut self, task: &EevdfEntity<T, S>) {
        self.curr = Some(Self::snapshot_of(task));
    }

    fn clear_curr_if(&mut self, task: &EevdfEntity<T, S>) {
        if self.curr.as_ref().is_some_and(|c| c.id == task.id()) {
            self.curr = None;
        }
    }

    fn refresh_curr_from(&mut self, task: &EevdfEntity<T, S>) {
        if self.curr.as_ref().is_some_and(|c| c.id == task.id()) {
            self.set_curr_from(task);
        }
    }

    /// Linux `set_preempt_buddy`: nominate `wakee` for the next pick, keeping
    /// an existing buddy that still has an earlier deadline.
    fn nominate_wake_buddy(&mut self, wakee: Arc<EevdfEntity<T, S>>) {
        // Only meaningful while a running task can hand off via block/yield.
        if self.curr.is_none() {
            return;
        }
        if let Some(existing) = self.next_buddy.as_ref()
            && Self::entity_before(existing, &wakee)
        {
            Self::stat_inc(
                &mut self.stats.wake_handoff_skipped_busy,
                self.stats_enabled,
            );
            return;
        }
        self.next_buddy = Some(wakee);
    }

    /// Prefer a one-shot wake buddy if it is still queued and eligible.
    ///
    /// Matches Linux `pick_eevdf` NEXT_BUDDY intent: the buddy must not sort
    /// *after* the earliest eligible ready entity (`entity_before(best, buddy)`
    /// → ignore hint). The hint is always consumed (`take`), so a discarded
    /// buddy is one-shot. [`Self::prefer_sync_buddy`] allows a later eligible
    /// buddy after `WF_SYNC` forced preemption (otherwise the just-requeued
    /// `curr` would win on deadline and undo the sync handoff).
    fn try_pick_wake_buddy(&mut self) -> Option<Arc<EevdfEntity<T, S>>> {
        let force_buddy = self.prefer_sync_buddy;
        self.prefer_sync_buddy = false;
        let buddy = self.next_buddy.take()?;

        let v = self.avg_vruntime();
        if buddy.vruntime() > v {
            return None;
        }

        if !force_buddy {
            let best_key = self.earliest_eligible_key(v)?;
            let best = self.ready_queue.get(&best_key)?;
            // Buddy loses to an earlier-deadline eligible peer: drop hint.
            // When buddy *is* best, neither sorts before the other → honour it.
            if Self::entity_before(best, &buddy) {
                return None;
            }
        }

        let key = (buddy.deadline(), buddy.id());
        let picked = self.dequeue_by_key(key)?;
        if Arc::ptr_eq(&picked, &buddy) {
            Some(picked)
        } else {
            // Defensive: key matched a different task; restore and ignore buddy.
            self.enqueue(picked);
            None
        }
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
        weighted_avg_vruntime(
            self.total_weighted_vrt,
            self.total_weight,
            current.vruntime(),
            current.weight(),
            self.min_vruntime,
        )
    }

    /// Placement **V**: ready-queue average plus [`Self::curr`] when set.
    ///
    /// Matches Linux `avg_vruntime()` accounting for `cfs_rq->curr`. When the
    /// ready queue is empty and only `curr` is runnable, **V** is
    /// `curr.vruntime`.
    fn system_vruntime(&self) -> isize {
        match self.curr {
            Some(curr) => weighted_avg_vruntime(
                self.total_weighted_vrt,
                self.total_weight,
                curr.vruntime,
                curr.weight,
                self.min_vruntime,
            ),
            None => self.avg_vruntime(),
        }
    }

    /// Weight sum for placement (ready queue plus `curr` when set).
    fn system_weight(&self) -> i128 {
        match self.curr {
            Some(curr) => self.total_weight + curr.weight as i128,
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
    /// `avg` / `load` are the caller's precomputed [`Self::system_vruntime`] /
    /// [`Self::system_weight`] so wake placement can reuse them for the
    /// eligibility / `min_vruntime` clamps without a second weighted average.
    /// Inflates lag by `(W + w) / W` before `vruntime = V - lag`, matching
    /// `kernel/sched/fair.c` `place_entity()`.
    fn place_waking_vruntime(&self, task: &EevdfEntity<T, S>, avg: isize, load: i128) -> isize {
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

    /// Linux `entity_before`: earlier virtual deadline wins (id tie-break).
    #[inline]
    fn entity_before(a: &EevdfEntity<T, S>, b: &EevdfEntity<T, S>) -> bool {
        deadline_before(a.deadline(), a.id(), b.deadline(), b.id())
    }

    /// Install the non-owning `curr` snapshot for a task that reached the CPU
    /// without [`BaseScheduler::pick_next_task`] (e.g. affinity migration
    /// helpers via `switch_to_local`).
    ///
    /// Call once at switch-in. While that task runs, [`Self::peer_preempts_curr`]
    /// uses this snapshot; [`BaseScheduler::leave_current`] clears it. Do not
    /// call from the preempt probe itself, and never for idle (idle must keep
    /// `curr` unset so a non-empty ready queue simply reports preemptable).
    /// Stores only values — never an owning `Arc`.
    pub fn sync_running_curr(&mut self, task: &EevdfEntity<T, S>) {
        self.set_curr_from(task);
    }

    /// Arm sync preemption after a `WF_SYNC` wake that nominated a next buddy.
    ///
    /// The next [`Self::peer_preempts_curr`] may then force a switch for an
    /// eligible buddy even when its deadline is later than `curr`.
    pub fn mark_sync_wake_preempt(&mut self) {
        if self.next_buddy.is_some() {
            self.sync_preempt_pending = true;
        }
    }

    /// Linux-style wakeup/tick preemption probe: would an eligible ready peer
    /// beat [`Self::curr`] on deadline **without** requeueing `curr` first?
    ///
    /// Callers that get `true` should [`BaseScheduler::leave_current`] with
    /// [`CurrentDisposition::Preempt`] then `pick_next`. Callers that get
    /// `false` must leave `curr` alone — requeueing it before pick is what
    /// created the short-deadline leapfrog tails.
    ///
    /// A pending sync wake (eligible next buddy) also returns `true`.
    ///
    /// If `curr` is still unset, returns whether any ready peer exists — correct
    /// for idle. Off-tree runners must already have been installed via
    /// [`Self::sync_running_curr`] at switch-in.
    pub fn peer_preempts_curr(&mut self) -> bool {
        let Some(curr) = self.curr else {
            self.sync_preempt_pending = false;
            return !self.ready_queue.is_empty();
        };
        if self.ready_queue.is_empty() {
            self.sync_preempt_pending = false;
            return false;
        }

        let v = self.system_vruntime();
        if self.sync_preempt_pending {
            self.sync_preempt_pending = false;
            if let Some(buddy) = self.next_buddy.as_ref()
                && buddy.vruntime() <= v
            {
                // Next pick must still prefer this buddy after leave(Preempt).
                self.prefer_sync_buddy = true;
                Self::stat_inc(&mut self.stats.wake_sync_preempt, self.stats_enabled);
                return true;
            }
        }

        let peer_key = match self.earliest_eligible_key(v) {
            Some(k) => k,
            None => *self.ready_queue.keys().next().unwrap(),
        };
        let peer = self.ready_queue.get(&peer_key).unwrap();
        let curr_eligible = curr.vruntime <= v;
        let curr_before_peer = deadline_before(curr.deadline, curr.id, peer.deadline(), peer.id());
        !(curr_eligible && curr_before_peer)
    }

    #[cfg(any(test, unittest))]
    pub(crate) fn curr_is_none(&self) -> bool {
        self.curr.is_none()
    }

    /// Refresh the non-owning `curr` snapshot after test-only entity mutations.
    #[cfg(any(test, unittest))]
    pub(crate) fn refresh_curr_snapshot_for_test(&mut self, task: &EevdfEntity<T, S>) {
        self.refresh_curr_from(task);
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
        self.clear_next_buddy_if(task);
        // Snapshot lag against the average that still includes this task and
        // the dequeued running task (`curr`), matching Linux avg_vruntime().
        let v = self.system_vruntime();
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

    fn pick_next_task(&mut self) -> Option<Self::SchedItem> {
        // Involuntary preemption must `leave_current(Preempt)` in ktask *after*
        // a [`Self::peer_preempts_curr`] probe, so `curr` is clear here.
        assert!(
            self.curr.is_none(),
            "pick_next_task requires leave_current to clear curr first"
        );

        if self.ready_queue.is_empty() {
            self.next_buddy = None;
            self.prefer_sync_buddy = false;
            return None;
        }

        if let Some(picked) = self.try_pick_wake_buddy() {
            Self::stat_inc(&mut self.stats.picks_total, self.stats_enabled);
            Self::stat_inc(&mut self.stats.wake_handoff, self.stats_enabled);
            self.set_curr_from(&picked);
            return Some(picked);
        }

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
        self.set_curr_from(&picked);
        Some(picked)
    }

    fn enqueue_task(&mut self, task: Self::SchedItem) {
        if task.needs_place() {
            // Wake / migrate-in: place by saved vlag, start a new request.
            // Do not clear `curr` — the running task on this rq is still someone else.
            let avg = self.system_vruntime();
            let load = self.system_weight();
            let placed = self.place_waking_vruntime(&task, avg, load);
            // Cap at system V (eligible), then floor at `min_vruntime` last so a
            // temporarily low V cannot punch through the ready-queue watermark.
            let vr = placed.min(avg).max(self.min_vruntime);
            task.set_vruntime(vr);
            // Linux `place_entity`: vd = ve + r/w with a full request slice.
            task.reset_slice();
            task.set_deadline(vr + deadline_delta(task.request_ticks(), task.weight()));
            task.set_needs_place(false);
            let wakee = task.clone();
            self.requeue(task);
            self.nominate_wake_buddy(wakee);
            return;
        }

        let vr = task.vruntime().max(self.min_vruntime);
        task.set_vruntime(vr);
        task.reset_slice();
        task.set_deadline(vr + deadline_delta(task.request_ticks(), task.weight()));
        self.requeue(task);
    }

    fn leave_current(&mut self, current: Self::SchedItem, disposition: CurrentDisposition) {
        self.clear_next_buddy_if(&current);
        self.clear_curr_if(&current);

        match disposition {
            CurrentDisposition::Yield => {
                let vr = current.vruntime().max(self.min_vruntime);
                current.set_vruntime(vr);
                current.reset_slice();
                current
                    .set_deadline(vr + deadline_delta(current.request_ticks(), current.weight()));
                self.requeue(current);
            }
            CurrentDisposition::Preempt => {
                let vr = current.vruntime().max(self.min_vruntime);
                current.set_vruntime(vr);
                if current.slice() > 0 {
                    if current.deadline() <= vr {
                        current.set_deadline(
                            vr + deadline_delta(current.slice() as usize, current.weight()),
                        );
                    }
                } else {
                    current.reset_slice();
                    current.set_deadline(
                        vr + deadline_delta(current.request_ticks(), current.weight()),
                    );
                }
                self.requeue(current);
            }
            CurrentDisposition::Block | CurrentDisposition::Migrate => {
                // Include the leaving task in V so lag matches Linux
                // `update_entity_lag` for curr.
                let v = self.avg_vruntime_with(&current);
                let lag = v - current.vruntime();
                current.set_vlag(Self::clamp_vlag(
                    lag,
                    current.weight(),
                    current.request_ticks(),
                ));
                current.set_needs_place(true);
            }
            CurrentDisposition::Exit => {
                // Do not arm PLACE_LAG; the task will never re-enter a RQ.
                current.set_needs_place(false);
            }
        }
    }

    fn task_tick(&mut self, current: &Self::SchedItem) -> bool {
        let delta = vruntime_delta(current.weight());
        current.vruntime.fetch_add(delta, Ordering::Release);

        let old_slice = current.slice.fetch_sub(1, Ordering::Release);
        self.refresh_curr_from(current);
        if old_slice <= 1 {
            Self::stat_inc(&mut self.stats.slice_expired, self.stats_enabled);
            return true;
        }

        // Same rule as pick_next: preempt for the earliest eligible deadline,
        // not only when the global min-deadline task happens to be eligible.
        // Also treat an expired request (`vruntime >= deadline`) as preemptable
        // when a peer is waiting — but never alone, or every short-deadline
        // wakee pays an extra resched after one tick and throughput collapses.
        let v = self.avg_vruntime_with(current);
        if let Some((dl, _)) = self.earliest_eligible_key(v)
            && (dl < current.deadline() || current.vruntime() >= current.deadline())
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
            self.refresh_curr_from(task);
        }

        true
    }
}

impl<T, const S: usize> Default for EevdfScheduler<T, S> {
    fn default() -> Self {
        Self::new()
    }
}

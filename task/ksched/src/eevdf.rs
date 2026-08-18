// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Weak},
};
use core::{
    ops::Deref,
    sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
};

use crate::{BaseScheduler, CurrentDisposition};

/// Nice-0 load. Dimensionless; at this weight 1 wall-clock ns = 1 vruntime unit.
const NICE_0_WEIGHT: i64 = 1024;

/// Linux-compatible nice-to-weight table.
/// Index 0 corresponds to nice -20; index 39 to nice 19.
const NICE_TO_WEIGHT: [i64; 40] = [
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

fn nice_to_weight(nice: i64) -> i64 {
    NICE_TO_WEIGHT[(nice + 20).clamp(0, 39) as usize]
}

/// Wall-clock ns → vruntime: `elapsed_ns × NICE_0_WEIGHT / weight`.
///
/// Higher weight ⇒ smaller delta ⇒ slower vruntime growth ⇒ more CPU share.
/// Also used for `deadline += vruntime_delta(request_ns, weight)` (`vd = ve + r/w`).
fn vruntime_delta(elapsed_ns: u64, weight: i64) -> i64 {
    if elapsed_ns == 0 || weight <= 0 {
        return 0;
    }
    let delta = elapsed_ns as i128 * NICE_0_WEIGHT as i128 / weight as i128;
    delta.clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

/// Vruntime gap → wall-clock ns at `weight`: `ceil(delta * weight / NICE_0_WEIGHT)`.
fn vruntime_to_wall_ns(delta_vruntime: i64, weight: i64) -> u64 {
    if delta_vruntime <= 0 || weight <= 0 {
        return 0;
    }
    let ns = (delta_vruntime as i128 * weight as i128 + (NICE_0_WEIGHT as i128 - 1))
        / NICE_0_WEIGHT as i128;
    ns.min(u64::MAX as i128) as u64
}

/// Load-weighted average vruntime: `(Σw·v + w_c·v_c) / (Σw + w_c)`.
///
/// Shared by ready-queue+running-entity **V** paths so Arc entities and
/// [`RunningEntitySnapshot`] cannot drift apart. `empty_fallback` is used when
/// the combined weight is non-positive (typically [`EevdfScheduler::min_vruntime`]).
#[inline]
fn weighted_avg_vruntime(
    queue_weighted_vruntime: i128,
    queue_weight: i64,
    curr_vruntime: i64,
    curr_weight: i64,
    empty_fallback: i64,
) -> i64 {
    let cw: i128 = curr_weight as i128;
    let wsum = queue_weighted_vruntime + curr_vruntime as i128 * cw;
    let wtot = queue_weight as i128 + cw;
    if wtot <= 0 {
        empty_fallback
    } else {
        (wsum / wtot) as i64
    }
}

/// Linux `entity_before` on raw `(deadline, id)` keys.
#[inline]
fn deadline_before(a_deadline: i64, a_id: u64, b_deadline: i64, b_id: u64) -> bool {
    a_deadline < b_deadline || (a_deadline == b_deadline && a_id < b_id)
}

/// Per-task EEVDF scheduling entity.
///
/// Virtual time (`vruntime`, `deadline`, `vlag`) is `i64`: same scale as
/// wall-clock nanoseconds at nice 0, signed so PLACE can go below 0.
/// Wall-clock remaining/request lengths are `u64` nanoseconds (`slice_ns`,
/// `request_ns`). Convert only through `vruntime_delta` / `vruntime_to_wall_ns`.
///
/// `MAX_SLICE_NS` is both the default and maximum request length in nanoseconds.
pub struct EevdfEntity<T, const MAX_SLICE_NS: usize> {
    inner: T,
    vruntime: AtomicI64,
    deadline: AtomicI64,
    /// Virtual lag `V - vruntime` saved when leaving the run queue (sleep).
    vlag: AtomicI64,
    /// When set, the next [`BaseScheduler::enqueue_task`] applies lag-based
    /// placement (wake / migrate-in after deactivate), not yield/preempt requeue.
    needs_place: AtomicBool,
    nice: AtomicI64,
    /// Remaining wall-clock nanoseconds in the current request.
    slice_ns: AtomicU64,
    /// Full request length in nanoseconds (defaults to `MAX_SLICE_NS`).
    request_ns: AtomicU64,
    id: AtomicU64,
}

impl<T, const S: usize> EevdfEntity<T, S> {
    pub const fn new(inner: T) -> Self {
        Self {
            inner,
            vruntime: AtomicI64::new(0),
            deadline: AtomicI64::new(0),
            vlag: AtomicI64::new(0),
            needs_place: AtomicBool::new(false),
            nice: AtomicI64::new(0),
            slice_ns: AtomicU64::new(S as u64),
            request_ns: AtomicU64::new(S as u64),
            id: AtomicU64::new(0),
        }
    }

    fn weight(&self) -> i64 {
        nice_to_weight(self.nice.load(Ordering::Acquire))
    }

    fn vruntime(&self) -> i64 {
        self.vruntime.load(Ordering::Acquire)
    }

    fn set_vruntime(&self, v: i64) {
        self.vruntime.store(v, Ordering::Release);
    }

    pub(crate) fn deadline(&self) -> i64 {
        self.deadline.load(Ordering::Acquire)
    }

    fn set_deadline(&self, d: i64) {
        self.deadline.store(d, Ordering::Release);
    }

    fn vlag(&self) -> i64 {
        self.vlag.load(Ordering::Acquire)
    }

    fn set_vlag(&self, lag: i64) {
        self.vlag.store(lag, Ordering::Release);
    }

    fn needs_place(&self) -> bool {
        self.needs_place.load(Ordering::Acquire)
    }

    fn set_needs_place(&self, needed: bool) {
        self.needs_place.store(needed, Ordering::Release);
    }

    fn id(&self) -> u64 {
        self.id.load(Ordering::Acquire)
    }

    fn set_id(&self, id: u64) {
        self.id.store(id, Ordering::Release);
    }

    pub(crate) fn slice_ns(&self) -> u64 {
        self.slice_ns.load(Ordering::Acquire)
    }

    fn request_ns(&self) -> u64 {
        self.request_ns.load(Ordering::Acquire).max(1)
    }

    /// Sets the request length used for new virtual deadlines.
    ///
    /// `ns` must be in `1..=MAX_SLICE_NS`. Shorter requests get earlier
    /// deadlines and are preferred among eligible tasks (Linux-style latency).
    pub fn set_request_ns(&self, ns: u64) -> bool {
        if !(1..=S as u64).contains(&ns) {
            return false;
        }
        self.request_ns.store(ns, Ordering::Release);
        true
    }

    pub(crate) fn reset_slice(&self) {
        self.slice_ns
            .store(self.request_ns.load(Ordering::Acquire), Ordering::Release);
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
    /// `mark_sync_wake_preempt` armed the flag (a next buddy was present).
    pub wake_sync_mark: u64,
    /// `WF_SYNC` mark saw no next buddy (nominate skipped or already consumed).
    pub wake_sync_mark_no_buddy: u64,
    /// Nominated a wake buddy while `curr` was unset (leave/idle window).
    pub wake_nominate_no_curr: u64,
    /// Probe had `sync_preempt_pending` but `next_buddy` was already gone.
    pub probe_sync_no_buddy: u64,
    /// Probe had the flag and a buddy whose vruntime was above system **V**.
    pub probe_sync_ineligible: u64,
    /// Probe returned false while a next buddy was still queued.
    pub probe_false_with_buddy: u64,
    /// `try_pick_wake_buddy` consumed the hint and then dropped it.
    pub buddy_pick_drop: u64,
}

impl EevdfStats {
    pub const fn zero() -> Self {
        Self {
            picks_total: 0,
            preempt_by_deadline: 0,
            fallback_no_eligible: 0,
            slice_expired: 0,
            wake_handoff: 0,
            wake_handoff_skipped_busy: 0,
            wake_sync_preempt: 0,
            wake_sync_mark: 0,
            wake_sync_mark_no_buddy: 0,
            wake_nominate_no_curr: 0,
            probe_sync_no_buddy: 0,
            probe_sync_ineligible: 0,
            probe_false_with_buddy: 0,
            buddy_pick_drop: 0,
        }
    }
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
/// use. Production paths do this in [`BaseScheduler::update_current`] and
/// [`BaseScheduler::set_priority`]; test-only mutators must call
/// `refresh_curr_snapshot_for_test`.
#[derive(Clone, Copy, Debug)]
struct RunningEntitySnapshot {
    id: u64,
    vruntime: i64,
    deadline: i64,
    weight: i64,
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
/// 1. **Placement** — Linux `place_entity` / `PLACE_LAG`: `vruntime = V - lag`
///    after inflating lag by `(W + w) / W`. Negative lag may place above **V**
///    (ineligible until **V** catches up); there is no extra cap/floor onto **V**
///    or `min_vruntime`.
/// 2. **Pick vs `curr`** — involuntary preemption probes with `curr` off-tree
///    (`peer_preempts_curr`); only an earlier-deadline eligible wakee forces
///    `leave_current(Preempt)`+`pick`, unless a sync wake armed an eligible next buddy.
/// 3. **Next buddy** — like Linux `NEXT_BUDDY` / `set_next_buddy`, a wake
///    nominates the wakee as a one-shot pick hint even when `curr` is unset
///    (leave→pick window / idle with other waiters). Existing buddies with an
///    earlier deadline are kept so concurrent wakes cannot leapfrog. Buddy pick
///    still requires eligibility.
/// 4. **WF_SYNC** — when the waker marks a sync wake (e.g. futex), an eligible
///    next buddy may preempt `curr` even with a later deadline, so the wakee
///    need not wait for the waker to block. Hint only: ineligible buddies never
///    force preemption.
/// 5. **Runtime** — [`Self::update_current`] matches Linux `update_deadline`:
///    when the request completes, assign a new `vd = ve + r/w` and reschedule
///    only if another task is waiting, so a lone task is not thrashed.
///
/// `MAX_SLICE_NS` is the default / maximum request length in nanoseconds.
pub struct EevdfScheduler<T, const MAX_SLICE_NS: usize> {
    /// Ready tasks keyed by `(deadline, id)`.
    ready_queue: BTreeMap<(i64, u64), Arc<EevdfEntity<T, MAX_SLICE_NS>>>,
    /// Secondary index keyed by `(vruntime, id)` for O(log N) min-vruntime and
    /// O(log N + E) eligible-task range queries.
    vrt_set: BTreeSet<(i64, u64)>,
    /// Reverse map from task id to its current deadline, used to look up the
    /// `ready_queue` key for tasks found via `vrt_set` range queries.
    id_to_deadline: BTreeMap<u64, i64>,
    /// Monotonic vruntime watermark (Linux `cfs_rq->min_vruntime`).
    ///
    /// Updated by [`Self::update_min_vruntime`]: off-tree but still-runnable
    /// `curr` participates, matching Linux `curr->on_rq`. Ready-only ratcheting
    /// would park the watermark on ineligible waiters above system **V**.
    min_vruntime: i64,
    /// Incrementally maintained `Σ w·v` for O(1) ready-queue `avg_vruntime`.
    /// Widened: `i64` vruntime × weight overflows `i64`.
    weighted_vruntime_sum: i128,
    total_weight: i64,
    /// Non-owning snapshot of the dequeued running task (Linux `cfs_rq->curr`).
    /// Included in placement **V** via [`Self::system_vruntime`]. Must stay in
    /// sync with the live running entity (see [`RunningEntitySnapshot`]).
    curr: Option<RunningEntitySnapshot>,
    /// Weak handle to the running entity for cross-CPU runtime flush before
    /// placement. Cleared with [`Self::curr`].
    curr_task: Option<Weak<EevdfEntity<T, MAX_SLICE_NS>>>,
    /// One-shot preferred wakee for the next pick (same run queue only).
    next_buddy: Option<Arc<EevdfEntity<T, MAX_SLICE_NS>>>,
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
}

impl<T, const S: usize> EevdfScheduler<T, S> {
    pub const fn new() -> Self {
        Self {
            ready_queue: BTreeMap::new(),
            vrt_set: BTreeSet::new(),
            id_to_deadline: BTreeMap::new(),
            min_vruntime: 0,
            weighted_vruntime_sum: 0,
            total_weight: 0,
            curr: None,
            curr_task: None,
            next_buddy: None,
            sync_preempt_pending: false,
            prefer_sync_buddy: false,
            id_pool: 0,
            stats_enabled: false,
            stats: EevdfStats::zero(),
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
        let w = task.weight();

        self.ready_queue.insert((dl, id), task);
        self.vrt_set.insert((vr, id));
        self.id_to_deadline.insert(id, dl);
        self.weighted_vruntime_sum += vr as i128 * w as i128;
        self.total_weight += w;
        // Linux updates the watermark on enqueue while `cfs_rq->curr` is still
        // the running entity. After `leave_current` we have already cleared
        // `curr` and then requeue; updating here would see only ready waiters
        // and park `min_vruntime` above V.
        if self.curr.is_some() {
            self.update_min_vruntime();
        }
    }

    fn dequeue_by_key(&mut self, key: (i64, u64)) -> Option<Arc<EevdfEntity<T, S>>> {
        let task = self.ready_queue.remove(&key)?;
        let id = task.id();
        let w = task.weight();
        let vr = task.vruntime();
        assert!(
            self.vrt_set.remove(&(vr, id)),
            "EEVDF vrt_set out of sync with ready_queue for task {id}"
        );

        self.id_to_deadline.remove(&id);
        self.weighted_vruntime_sum -= vr as i128 * w as i128;
        self.total_weight -= w;
        Some(task)
    }

    /// Linux `update_min_vruntime`: never let the watermark jump onto an
    /// ineligible ready peer while a lower-vruntime `curr` is still running.
    ///
    /// `curr` is off the ready tree but still runnable (Linux `cfs_rq->curr`
    /// with `on_rq`). Candidate is `curr.vruntime` when set, then min'd with
    /// the ready-tree minimum vruntime (EEVDF stand-in for CFS leftmost). The
    /// stored watermark only ratchets forward.
    ///
    /// Call after tree composition or `curr` identity changes (`enqueue` while
    /// `curr` is set, `install_curr`, `remove_task`, and `leave_current` *before*
    /// clearing `curr`). Do not call from pick's tree-only dequeue (Linux
    /// `__dequeue_entity` / `set_next_entity`), and do not call after
    /// `curr` is cleared while ineligible waiters remain: that parks the
    /// watermark above **V** with no way to ratchet back. `leave_current`
    /// requeue is such a window.
    fn update_min_vruntime(&mut self) {
        let mut vruntime = self.min_vruntime;
        if let Some(curr) = self.curr {
            vruntime = curr.vruntime;
        }
        if let Some(&(min_vr, _)) = self.vrt_set.iter().next() {
            vruntime = if self.curr.is_none() {
                min_vr
            } else {
                vruntime.min(min_vr)
            };
        }
        self.min_vruntime = self.min_vruntime.max(vruntime);
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

    fn install_curr(&mut self, task: &Arc<EevdfEntity<T, S>>) {
        self.set_curr_from(task);
        self.curr_task = Some(Arc::downgrade(task));
        self.update_min_vruntime();
    }

    fn clear_curr_if(&mut self, task: &EevdfEntity<T, S>) {
        if self.curr.as_ref().is_some_and(|c| c.id == task.id()) {
            self.curr = None;
            self.curr_task = None;
        }
    }

    fn refresh_curr_from(&mut self, task: &EevdfEntity<T, S>) {
        if self.curr.as_ref().is_some_and(|c| c.id == task.id()) {
            self.set_curr_from(task);
        }
    }

    /// Linux `set_next_buddy`: nominate `wakee` for the next pick, keeping
    /// an existing buddy that still has an earlier deadline.
    ///
    /// Must still nominate when [`Self::curr`] is unset. Remote WF_SYNC often
    /// enqueues in the leave→pick gap (or onto idle with other waiters already
    /// queued); skipping here leaves `mark_no_buddy` and the next pick keeps
    /// the earlier-deadline runner for a full request (~2ms schbench p99.9).
    fn nominate_wake_buddy(&mut self, wakee: Arc<EevdfEntity<T, S>>) {
        if self.curr.is_none() {
            Self::stat_inc(&mut self.stats.wake_nominate_no_curr, self.stats_enabled);
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
            Self::stat_inc(&mut self.stats.buddy_pick_drop, self.stats_enabled);
            return None;
        }

        if !force_buddy {
            let Some(best_key) = self.earliest_eligible_key(v) else {
                Self::stat_inc(&mut self.stats.buddy_pick_drop, self.stats_enabled);
                return None;
            };
            let Some(best) = self.ready_queue.get(&best_key) else {
                Self::stat_inc(&mut self.stats.buddy_pick_drop, self.stats_enabled);
                return None;
            };
            // Buddy loses to an earlier-deadline eligible peer: drop hint.
            // When buddy *is* best, neither sorts before the other → honour it.
            if Self::entity_before(best, &buddy) {
                Self::stat_inc(&mut self.stats.buddy_pick_drop, self.stats_enabled);
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
            Self::stat_inc(&mut self.stats.buddy_pick_drop, self.stats_enabled);
            None
        }
    }

    // ---- virtual time ----

    /// Ready-queue-only weighted average **V** (excludes [`Self::curr`]).
    /// Used by `pick_next` after the previous running task has been put back
    /// or blocked, when `curr` is already clear.
    fn avg_vruntime(&self) -> i64 {
        if self.total_weight <= 0 {
            self.min_vruntime
        } else {
            (self.weighted_vruntime_sum / self.total_weight as i128) as i64
        }
    }

    /// V that additionally includes a currently-running task which has been
    /// removed from the ready queue.  O(1).
    fn avg_vruntime_with(&self, current: &EevdfEntity<T, S>) -> i64 {
        weighted_avg_vruntime(
            self.weighted_vruntime_sum,
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
    fn system_vruntime(&self) -> i64 {
        match self.curr {
            Some(curr) => weighted_avg_vruntime(
                self.weighted_vruntime_sum,
                self.total_weight,
                curr.vruntime,
                curr.weight,
                self.min_vruntime,
            ),
            None => self.avg_vruntime(),
        }
    }

    /// Weight sum for placement (ready queue plus `curr` when set).
    fn system_weight(&self) -> i64 {
        match self.curr {
            Some(curr) => self.total_weight + curr.weight,
            None => self.total_weight,
        }
    }

    /// Clamp |vlag| to about two requests at the task's weight.
    ///
    /// Linux `update_entity_lag` uses `max(2 * slice, TICK_NSEC)` converted to
    /// virtual time. Without a periodic tick the `TICK_NSEC` floor is 1 ns.
    fn clamp_vlag(lag: i64, weight: i64, request_ns: u64) -> i64 {
        let limit_ns = request_ns.saturating_mul(2).max(1);
        let limit = vruntime_delta(limit_ns, weight);
        lag.clamp(-limit, limit)
    }

    /// Linux `PLACE_LAG`: place a waking task so its virtual lag is preserved
    /// after it joins the weighted average.
    ///
    /// `avg` / `load` are the caller's precomputed [`Self::system_vruntime`] /
    /// [`Self::system_weight`]. Inflates lag by `(W + w) / W` before
    /// `vruntime = V - lag`, matching `kernel/sched/fair.c` `place_entity()`.
    /// Does not clamp onto **V** or `min_vruntime`: a wakee that ran ahead of
    /// fair share may be ineligible until **V** catches up.
    fn place_waking_vruntime(&self, task: &EevdfEntity<T, S>, avg: i64, load: i64) -> i64 {
        if load <= 0 {
            return self.min_vruntime;
        }

        let weight = task.weight();
        let mut lag = Self::clamp_vlag(task.vlag(), weight, task.request_ns()) as i128;
        // vl = (W + w) * vl' / W
        lag = lag * (load as i128 + weight as i128) / load as i128;
        (avg as i128 - lag) as i64
    }

    /// Earliest-deadline key among tasks with `vruntime ≤ v`, if any.
    ///
    /// Do not walk deadline order: PLACE_LAG negative lag parks ineligible
    /// tasks at the front, so a deep ready queue would be O(n) on every pick
    /// and preemption probe. The min-deadline task is still O(1) when it is
    /// eligible; otherwise `vrt_set` ranges eligible entities in O(log n + E).
    fn earliest_eligible_key(&self, v: i64) -> Option<(i64, u64)> {
        let (&first_key, first_task) = self.ready_queue.first_key_value()?;
        if first_task.vruntime() <= v {
            return Some(first_key);
        }

        self.vrt_set
            .range(..=(v, u64::MAX))
            .filter_map(|&(_, id)| self.id_to_deadline.get(&id).copied().map(|dl| (dl, id)))
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
    pub fn sync_running_curr(&mut self, task: &Arc<EevdfEntity<T, S>>) {
        self.install_curr(task);
    }

    /// Charge wall time to the cached running entity (if still alive).
    ///
    /// Used by `ktask` before wake/new-task placement so a NOHZ lone runner's
    /// elapsed time is reflected in system **V** before PLACE_LAG.
    pub fn account_curr_elapsed(&mut self, elapsed_ns: u64) {
        if elapsed_ns == 0 {
            return;
        }
        let Some(task) = self.curr_task.as_ref().and_then(|w| w.upgrade()) else {
            return;
        };
        let _ = self.update_current(&task, elapsed_ns);
    }

    /// Linux `update_deadline`: start a new request when the current one is done.
    ///
    /// Completes when `vruntime >= deadline` (virtual request) or the remaining
    /// wall-clock slice hits 0 (`r_i` consumed). Assigns `vd = ve + r/w` and a
    /// fresh slice. Returns whether a waiting peer should get a scheduling turn
    /// (`nr_queued > 1` in Linux `update_curr`).
    fn update_deadline(&mut self, current: &EevdfEntity<T, S>) -> bool {
        let request_done = current.vruntime() >= current.deadline() || current.slice_ns() == 0;
        if !request_done {
            return false;
        }
        current.reset_slice();
        current.set_deadline(
            current.vruntime() + vruntime_delta(current.request_ns(), current.weight()),
        );
        self.refresh_curr_from(current);
        Self::stat_inc(&mut self.stats.slice_expired, self.stats_enabled);
        !self.ready_queue.is_empty()
    }

    /// Arm sync preemption after a `WF_SYNC` wake that nominated a next buddy.
    ///
    /// Also sets [`Self::prefer_sync_buddy`] immediately. A remote CPU can
    /// `leave`+`pick` (slice expired) before the IPI probe; without the pick
    /// preference the buddy hint is dropped and the wakee waits another request.
    pub fn mark_sync_wake_preempt(&mut self) {
        let Some(buddy) = self.next_buddy.as_ref() else {
            Self::stat_inc(&mut self.stats.wake_sync_mark_no_buddy, self.stats_enabled);
            return;
        };
        self.sync_preempt_pending = true;
        let v = self.system_vruntime();
        if buddy.vruntime() <= v {
            self.prefer_sync_buddy = true;
        }
        Self::stat_inc(&mut self.stats.wake_sync_mark, self.stats_enabled);
    }

    /// Peek: a `WF_SYNC` mark is still waiting (buddy may still be ineligible).
    ///
    /// Does not consume the mark. Timer IRQ uses [`Self::check_preempt_tick`],
    /// not this, to decide `need_resched`.
    pub fn sync_wake_pending(&self) -> bool {
        self.sync_preempt_pending && self.next_buddy.is_some()
    }

    /// Linux `check_preempt_tick` / `pick_eevdf` without consuming WF_SYNC.
    ///
    /// Returns whether `resched_curr` would fire: an eligible NEXT_BUDDY
    /// (even with a later deadline), or an eligible earlier-deadline peer.
    /// Does not mutate scheduler state — [`Self::peer_preempts_curr`] is what
    /// consumes the mark at `schedule()` time.
    ///
    /// Ineligible waiters return `false`; [`Self::next_preemption_ns`] arms
    /// until-eligible for a WF_SYNC buddy so the next tick can ask again
    /// once **V** catches up.
    pub fn check_preempt_tick(&mut self) -> bool {
        if self.ready_queue.is_empty() {
            return false;
        }

        let v = self.live_avg_vruntime();
        if self.sync_preempt_pending
            && let Some(buddy) = self.next_buddy.as_ref()
            && buddy.vruntime() <= v
        {
            return true;
        }

        let Some(curr) = self.curr else {
            return true;
        };
        let Some(peer_key) = self.earliest_eligible_key(v) else {
            return false;
        };
        let peer = self.ready_queue.get(&peer_key).unwrap();
        let curr_eligible = curr.vruntime <= v;
        let curr_before_peer = deadline_before(curr.deadline, curr.id, peer.deadline(), peer.id());
        let peer_wins = !(curr_eligible && curr_before_peer);
        if peer_wins {
            Self::stat_inc(&mut self.stats.preempt_by_deadline, self.stats_enabled);
        }
        peer_wins
    }

    /// **V** from the live running entity, same source as the WF_SYNC probe.
    fn live_avg_vruntime(&self) -> i64 {
        match self.curr_task.as_ref().and_then(|w| w.upgrade()) {
            Some(task) => self.avg_vruntime_with(&task),
            None => self.system_vruntime(),
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
    /// The sync flag is consumed only when that path actually wins; a failed
    /// probe must not drop it, or a later IPI/timer probe cannot retry.
    ///
    /// If `curr` is still unset, returns whether any ready peer exists — correct
    /// for idle. Off-tree runners must already have been installed via
    /// [`Self::sync_running_curr`] at switch-in.
    pub fn peer_preempts_curr(&mut self) -> bool {
        if self.ready_queue.is_empty() {
            self.sync_preempt_pending = false;
            return false;
        }

        let v = self.live_avg_vruntime();
        if self.sync_preempt_pending {
            match self.next_buddy.as_ref() {
                Some(buddy) if buddy.vruntime() <= v => {
                    self.sync_preempt_pending = false;
                    self.prefer_sync_buddy = true;
                    Self::stat_inc(&mut self.stats.wake_sync_preempt, self.stats_enabled);
                    return true;
                }
                Some(_) => {
                    Self::stat_inc(&mut self.stats.probe_sync_ineligible, self.stats_enabled);
                }
                None => {
                    Self::stat_inc(&mut self.stats.probe_sync_no_buddy, self.stats_enabled);
                }
            }
        }

        let Some(curr) = self.curr else {
            return true;
        };

        let peer_key = match self.earliest_eligible_key(v) {
            Some(k) => k,
            None => *self.ready_queue.keys().next().unwrap(),
        };
        let peer = self.ready_queue.get(&peer_key).unwrap();
        let curr_eligible = curr.vruntime <= v;
        let curr_before_peer = deadline_before(curr.deadline, curr.id, peer.deadline(), peer.id());
        let peer_wins = !(curr_eligible && curr_before_peer);
        if !peer_wins && self.next_buddy.is_some() {
            Self::stat_inc(&mut self.stats.probe_false_with_buddy, self.stats_enabled);
        }
        peer_wins
    }

    fn requeue(&mut self, task: Arc<EevdfEntity<T, S>>) {
        let id = self.next_id();
        task.set_id(id);
        self.enqueue(task);
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
        task.set_deadline(vr + vruntime_delta(task.request_ns(), task.weight()));
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
            removed.request_ns(),
        ));
        removed.set_needs_place(true);
        self.update_min_vruntime();
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
            self.install_curr(&picked);
            return Some(picked);
        }

        let v = self.avg_vruntime();
        let mut used_fallback = false;
        let first_key = *self.ready_queue.keys().next().unwrap();
        let eligible_key = self.earliest_eligible_key(v);

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
        self.install_curr(&picked);
        Some(picked)
    }

    fn enqueue_task(&mut self, task: Self::SchedItem) {
        if task.needs_place() {
            // Wake / migrate-in: place by saved vlag, start a new request.
            // Do not clear `curr` — the running task on this rq is still someone else.
            let avg = self.system_vruntime();
            let load = self.system_weight();
            let placed = self.place_waking_vruntime(&task, avg, load);
            // Linux `place_entity`: `se->vruntime = vruntime - lag` with no
            // extra cap onto V or floor at min_vruntime.
            task.set_vruntime(placed);
            // Linux `place_entity`: vd = ve + r/w with a full request slice.
            // Preempt decisions come from pick-vs-curr, not a fake short deadline.
            task.reset_slice();
            task.set_deadline(placed + vruntime_delta(task.request_ns(), task.weight()));
            task.set_needs_place(false);
            let wakee = task.clone();
            self.requeue(task);
            self.nominate_wake_buddy(wakee);
            return;
        }

        let vr = task.vruntime().max(self.min_vruntime);
        task.set_vruntime(vr);
        task.reset_slice();
        task.set_deadline(vr + vruntime_delta(task.request_ns(), task.weight()));
        self.requeue(task);
    }

    fn leave_current(&mut self, current: Self::SchedItem, disposition: CurrentDisposition) {
        self.clear_next_buddy_if(&current);
        // Linux `put_prev_entity`: `update_curr` (watermark with `curr` set)
        // then drop `cfs_rq->curr`. Updating after clear would see only
        // ineligible ready waiters and park min_vruntime above V.
        self.set_curr_from(&current);
        self.update_min_vruntime();
        self.clear_curr_if(&current);

        match disposition {
            CurrentDisposition::Yield => {
                let vr = current.vruntime().max(self.min_vruntime);
                current.set_vruntime(vr);
                current.reset_slice();
                current.set_deadline(vr + vruntime_delta(current.request_ns(), current.weight()));
                self.requeue(current);
            }
            CurrentDisposition::Preempt => {
                let vr = current.vruntime().max(self.min_vruntime);
                current.set_vruntime(vr);
                if current.slice_ns() > 0 {
                    if current.deadline() <= vr {
                        current.set_deadline(
                            vr + vruntime_delta(current.slice_ns(), current.weight()),
                        );
                    }
                } else {
                    current.reset_slice();
                    current
                        .set_deadline(vr + vruntime_delta(current.request_ns(), current.weight()));
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
                    current.request_ns(),
                ));
                current.set_needs_place(true);
            }
            CurrentDisposition::Exit => {
                // Do not arm PLACE_LAG; the task will never re-enter a RQ.
                current.set_needs_place(false);
            }
        }
    }

    fn update_current(&mut self, current: &Self::SchedItem, elapsed_ns: u64) -> bool {
        if elapsed_ns > 0 {
            let delta = vruntime_delta(elapsed_ns, current.weight());
            current.vruntime.fetch_add(delta, Ordering::Release);

            let old_slice = current.slice_ns.load(Ordering::Acquire);
            let consumed = elapsed_ns.min(old_slice);
            current
                .slice_ns
                .store(old_slice.saturating_sub(consumed), Ordering::Release);
            self.refresh_curr_from(current);
        }

        if self.update_deadline(current) {
            return true;
        }

        // Peer deadline comparison belongs in [`Self::check_preempt_tick`] /
        // [`Self::peer_preempts_curr`], not here. Linux `update_curr` only
        // rescheds when this request is done. Doing it on every account yanked
        // a WF_SYNC later-deadline wakee back to the previous runner.
        false
    }

    fn next_preemption_ns(&self, current: &Self::SchedItem) -> Option<u64> {
        // Lone runnable task: no schedule timer (wake paths force re-evaluation).
        if self.ready_queue.is_empty() {
            return None;
        }

        let mut next = current.slice_ns();

        // Wall time until current virtual deadline is reached.
        let weight = current.weight();
        let vr = current.vruntime();
        let dl = current.deadline();
        if dl <= vr {
            return Some(0);
        }
        next = next.min(vruntime_to_wall_ns(dl - vr, weight));

        // Until the WF_SYNC buddy becomes eligible. Do not poll every
        // ineligible waiter at the 10µs hrtick floor — that livelocks busy
        // CPUs and ping-pongs after NEXT_BUDDY. Other waiters wait for this
        // request; wake/IPI still preempts an already-eligible earlier peer.
        // **V** advances at NICE_0_WEIGHT / W, so convert the vruntime gap
        // with the combined ready+curr weight, not the running task's weight.
        let v = self.avg_vruntime_with(current);
        let wtot = self.total_weight.saturating_add(weight);
        if self.sync_preempt_pending
            && let Some(buddy) = self.next_buddy.as_ref()
            && buddy.vruntime() > v
            && wtot > 0
        {
            next = next.min(vruntime_to_wall_ns(buddy.vruntime() - v, wtot));
        }

        Some(next)
    }

    fn set_priority(&mut self, task: &Self::SchedItem, prio: isize) -> bool {
        if !(-20..=19).contains(&prio) {
            return false;
        }

        let old_weight = task.weight();
        if let Some(removed) = self.dequeue_by_key((task.deadline(), task.id())) {
            removed.nice.store(prio as i64, Ordering::Release);
            let new_weight = removed.weight();
            // Keep virtual lag consistent with weight-scaled lag when nice changes.
            if new_weight != old_weight && new_weight != 0 {
                let scaled = removed.vlag() as i128 * old_weight as i128 / new_weight as i128;
                removed.set_vlag(scaled as i64);
            }
            let vr = removed.vruntime();
            let remaining_ns = removed.slice_ns().max(1);
            removed.set_deadline(vr + vruntime_delta(remaining_ns, new_weight));
            self.requeue(removed);
        } else {
            task.nice.store(prio as i64, Ordering::Release);
            let new_weight = task.weight();
            if new_weight != old_weight && new_weight != 0 {
                let scaled = task.vlag() as i128 * old_weight as i128 / new_weight as i128;
                task.set_vlag(scaled as i64);
            }
            let remaining_ns = task.slice_ns().max(1);
            task.set_deadline(task.vruntime() + vruntime_delta(remaining_ns, new_weight));
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

/// Fixtures for `src/tests.rs`. Algorithm cases live there; this file only
/// exposes construction/inspection that tests cannot reach through
/// [`BaseScheduler`].
#[cfg(unittest)]
impl<T, const S: usize> EevdfEntity<T, S> {
    pub(crate) fn set_deadline_for_test(&self, d: i64) {
        self.set_deadline(d);
    }

    pub(crate) fn set_vruntime_for_test(&self, v: i64) {
        self.set_vruntime(v);
    }

    pub(crate) fn vruntime_for_test(&self) -> i64 {
        self.vruntime()
    }

    pub(crate) fn vlag_for_test(&self) -> i64 {
        self.vlag()
    }

    pub(crate) fn slice_for_test(&self) -> u64 {
        self.slice_ns()
    }

    pub(crate) fn needs_place_for_test(&self) -> bool {
        self.needs_place()
    }

    pub(crate) fn set_vlag_for_test(&self, lag: i64) {
        self.set_vlag(lag);
    }

    pub(crate) fn set_needs_place_for_test(&self, needed: bool) {
        self.set_needs_place(needed);
    }
}

#[cfg(unittest)]
impl<T, const S: usize> EevdfScheduler<T, S> {
    pub(crate) fn curr_is_none(&self) -> bool {
        self.curr.is_none()
    }

    /// After test-only mutation of a running entity, refresh the `curr` snapshot.
    pub(crate) fn refresh_curr_snapshot_for_test(&mut self, task: &EevdfEntity<T, S>) {
        self.refresh_curr_from(task);
    }

    /// Insert with the entity's current metadata (skip PLACE).
    pub(crate) fn inject_ready_for_test(&mut self, task: Arc<EevdfEntity<T, S>>) {
        task.set_needs_place(false);
        self.requeue(task);
    }

    pub(crate) fn min_vruntime_for_test(&self) -> i64 {
        self.min_vruntime
    }

    pub(crate) fn system_vruntime_for_test(&self) -> i64 {
        self.system_vruntime()
    }
}

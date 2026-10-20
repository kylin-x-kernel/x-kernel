// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Scheduled task: caller inner data plus class, attribute, and Fair state.

use core::{
    cell::UnsafeCell,
    ops::Deref,
    sync::atomic::{AtomicU8, AtomicU64, Ordering},
};

use intrusive_collections::RBTreeAtomicLink;

use crate::{
    policy::{Nice, RtPrio, SchedAttr, SchedClass, SchedError, SchedPolicy},
    ready_list::{ReadyItem, ReadyLinks},
};

/// Zero-sized token: RQ-protected fields may be read or written.
///
/// Constructed by [`SchedTask::init_attr_unpublished`] for a still-private
/// task, or later by a run-queue owner that already holds the RQ lock.
#[derive(Clone, Copy)]
pub(crate) struct RqLocked(());

impl RqLocked {
    /// # Safety
    ///
    /// The caller holds the owning run-queue lock for this task (or is the
    /// exclusive creator of an unpublished task).
    pub(crate) const unsafe fn assume() -> Self {
        Self(())
    }
}

/// Interior cell mutated only while the owning RQ lock is held.
#[repr(transparent)]
pub(crate) struct RqCell<V>(UnsafeCell<V>);

// SAFETY: every `RqCell` method is on `impl<V: Copy>`, and writes go through
// `RqLocked`. Concurrent readers without the token are not provided.
// `V: Send + Copy` matches that API: `Send` lets the value move with the
// lock across CPUs; `Copy` is what the cell actually stores.
unsafe impl<V: Send + Copy> Sync for RqCell<V> {}

impl<V: Copy> RqCell<V> {
    pub(crate) const fn new(v: V) -> Self {
        Self(UnsafeCell::new(v))
    }

    pub(crate) fn get(&self, _: RqLocked) -> V {
        // SAFETY: deref of `UnsafeCell::get()` (`*mut V`) for a read.
        // Validity and alignment: `RqCell` is `#[repr(transparent)]` over
        // `UnsafeCell<V>`, so `get()` yields a pointer to this cell's `V`.
        // Initialization: `new` stores a fully initialized `V`; `set` only
        // replaces it with another complete `V`.
        // No concurrent write: `RqLocked` means the RQ lock is held, so no
        // other CPU may `set` this cell. Exclusive write access is not
        // required for a `Copy` load.
        unsafe { *self.0.get() }
    }

    pub(crate) fn set(&self, v: V, _: RqLocked) {
        // SAFETY: write through `UnsafeCell::get()` (`*mut V`).
        // Validity and alignment: same transparent layout as `get`.
        // Initialization: the destination is a live `V` from `new` or a
        // previous `set`; the stored value is a complete `Copy` `V`.
        // Exclusive write: `RqLocked` means the RQ lock is held, so no
        // concurrent `get` or `set` on this cell.
        unsafe {
            *self.0.get() = v;
        }
    }

    pub(crate) fn update(&self, tok: RqLocked, f: impl FnOnce(V) -> V) {
        let next = f(self.get(tok));
        self.set(next, tok);
    }
}

/// Which ready structure currently holds this task.
///
/// `Detached` is 0 so it never collides with a class seat. The class
/// variants follow the same rank order as [`SchedClass`]
/// (Stop > Deadline > Rt > Fair > Idle), offset by 1. Do not use
/// [`SchedClass::as_u8`] as a seat byte; convert with
/// [`QueueSeat::from_sched_class`].
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QueueSeat {
    Detached = 0,
    Stop     = 1,
    Deadline = 2,
    Rt       = 3,
    Fair     = 4,
    Idle     = 5,
}

impl QueueSeat {
    pub(crate) const fn from_u8(raw: u8) -> Self {
        match raw {
            1 => Self::Stop,
            2 => Self::Deadline,
            3 => Self::Rt,
            4 => Self::Fair,
            5 => Self::Idle,
            _ => Self::Detached,
        }
    }

    /// Seat for `c`'s class queue. Not [`SchedClass::as_u8`]: that encoding
    /// has no `Detached` and would map Fair (3) onto [`Self::Rt`].
    pub(crate) const fn from_sched_class(c: SchedClass) -> Self {
        match c {
            SchedClass::Stop => Self::Stop,
            SchedClass::Deadline => Self::Deadline,
            SchedClass::Rt => Self::Rt,
            SchedClass::Fair => Self::Fair,
            SchedClass::Idle => Self::Idle,
        }
    }

    pub(crate) const fn as_u8(self) -> u8 {
        self as u8
    }
}

const _: () = {
    assert!(QueueSeat::from_sched_class(SchedClass::Fair).as_u8() == QueueSeat::Fair as u8);
    assert!(QueueSeat::from_u8(0).as_u8() == QueueSeat::Detached as u8);
    assert!(QueueSeat::Fair.as_u8() != SchedClass::Fair.as_u8());
};

/// EEVDF virtual-time fields and ready-tree links.
///
/// Present on every [`SchedTask`], including Idle. Only Fair reads and
/// writes it. Links are prepared here so enqueue does not allocate an
/// index node.
pub(crate) struct FairEntity {
    pub(crate) vruntime: RqCell<i64>,
    pub(crate) deadline: RqCell<i64>,
    pub(crate) vlag: RqCell<i64>,
    pub(crate) slice_left_ns: RqCell<u64>,
    pub(crate) request_ns: RqCell<u64>,
    pub(crate) weight: RqCell<u32>,
    pub(crate) tree_id: RqCell<u64>,
    /// Link in the deadline-ordered ready tree.
    pub(crate) deadline_link: RBTreeAtomicLink,
    /// Link in the vruntime-ordered tree used for the eligible prefix.
    pub(crate) vruntime_link: RBTreeAtomicLink,
}

/// FIFO/RR leftover quantum. Present on every task so a class switch does not allocate.
pub(crate) struct RtEntity {
    /// Nanoseconds left in the current `SCHED_RR` quantum.
    pub rr_left_ns: RqCell<u64>,
    /// This task's quantum expired with a same-priority waiter waiting.
    ///
    /// The next preempt of this task goes to the tail. Stored on the task so a
    /// class switch cannot leave the flag for a later RT runner.
    pub rr_rotate: RqCell<bool>,
}

impl RtEntity {
    const fn new(rr_slice_ns: u64) -> Self {
        Self {
            rr_left_ns: RqCell::new(rr_slice_ns),
            rr_rotate: RqCell::new(false),
        }
    }
}

/// CBS/EDF fields and the ready/throttled tree links for a Deadline task.
///
/// Present on every [`SchedTask`]; only the Deadline class reads or writes
/// it. Sibling of [`FairEntity`] so the class state stays next to the entity
/// and enqueue never allocates an index node.
///
/// The two links are separate because each residency is ordered by its own
/// instant: a ready task by its absolute deadline (`deadline_ns`), a throttled
/// task by the start of its next period (`period_start_ns`). Keeping them
/// apart also keeps `nr_queued` equal to the ready count.
pub(crate) struct DeadlineEntity {
    /// Budget left in the current period; charging stops at zero.
    ///
    /// `runtime_ns` / `period_ns` / `deadline_ns` are not duplicated here:
    /// they live in `SchedAttr`, and only the derived instants below are
    /// stored.
    pub(crate) budget_left_ns: RqCell<u64>,
    /// Absolute nanosecond instant this task must finish its request by.
    pub(crate) deadline_ns: RqCell<u64>,
    /// Absolute instant the next period starts, used as the replenish instant
    /// and as the throttled-tree key.
    pub(crate) period_start_ns: RqCell<u64>,
    /// `true` while the task holds a request; cleared when a check finds the
    /// deadline already passed, which drops its reservation.
    pub(crate) is_active: RqCell<bool>,
    /// `true` while the request's runtime is spent and the task waits for
    /// [`Self::period_start_ns`].
    pub(crate) is_throttled: RqCell<bool>,
    /// Tie-breaker for equal tree keys, assigned per enqueue by the class.
    pub(crate) tree_id: RqCell<u64>,
    /// Link in the deadline-ordered ready tree.
    pub(crate) ready_link: RBTreeAtomicLink,
    /// Link in the period-start-ordered throttled tree.
    pub(crate) throttled_link: RBTreeAtomicLink,
}

impl DeadlineEntity {
    const fn new() -> Self {
        Self {
            budget_left_ns: RqCell::new(0),
            deadline_ns: RqCell::new(0),
            period_start_ns: RqCell::new(0),
            is_active: RqCell::new(false),
            is_throttled: RqCell::new(false),
            tree_id: RqCell::new(0),
            ready_link: RBTreeAtomicLink::new(),
            throttled_link: RBTreeAtomicLink::new(),
        }
    }
}

impl FairEntity {
    const fn new(request_ns: u64) -> Self {
        Self {
            vruntime: RqCell::new(0),
            deadline: RqCell::new(0),
            vlag: RqCell::new(0),
            slice_left_ns: RqCell::new(request_ns),
            request_ns: RqCell::new(request_ns),
            weight: RqCell::new(Nice::DEFAULT.weight()),
            tree_id: RqCell::new(0),
            deadline_link: RBTreeAtomicLink::new(),
            vruntime_link: RBTreeAtomicLink::new(),
        }
    }
}

/// Kernel-only class used at task construction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KernelClass {
    /// Per-CPU idle thread.
    Idle,
}

impl KernelClass {
    const fn sched_class(self) -> SchedClass {
        match self {
            Self::Idle => SchedClass::Idle,
        }
    }
}

/// Scheduled task: caller-owned inner data plus class and attributes.
pub struct SchedTask<T> {
    inner: T,
    class: AtomicU8,
    /// Lock-free RT priority for CPU placement. `0` when not RT.
    rt_prio: AtomicU8,
    // Written only by the owning RQ's charge path; atomic for remote readers.
    sum_exec_runtime_ns: AtomicU64,
    seat: RqCell<QueueSeat>,
    attr: RqCell<SchedAttr>,
    /// `fair.rs` names this field in `offset_of!`. Other reads go through [`Self::fair`].
    pub(crate) fair: FairEntity,
    rt: RtEntity,
    /// Intrusive FIFO link for one RT priority list. Detached while the task
    /// is not queued on RT.
    rt_links: ReadyLinks,
    pub(crate) deadline: DeadlineEntity,
}

impl<T> SchedTask<T> {
    /// Wraps `inner` as a Fair task with [`SchedAttr::DEFAULT`].
    pub const fn new(inner: T) -> Self {
        Self::new_with_class(inner, SchedClass::Fair, SchedAttr::DEFAULT)
    }

    /// Wraps `inner` as a kernel-only Idle task.
    pub const fn new_kernel(inner: T, class: KernelClass) -> Self {
        Self::new_with_class(inner, class.sched_class(), SchedAttr::DEFAULT)
    }

    const fn new_with_class(inner: T, class: SchedClass, attr: SchedAttr) -> Self {
        Self {
            inner,
            class: AtomicU8::new(class.as_u8()),
            rt_prio: AtomicU8::new(attr.rt_priority.get()),
            sum_exec_runtime_ns: AtomicU64::new(0),
            seat: RqCell::new(QueueSeat::Detached),
            attr: RqCell::new(attr),
            fair: FairEntity::new(crate::DEFAULT_SLICE_NS),
            rt: RtEntity::new(crate::DEFAULT_RR_SLICE_NS),
            rt_links: ReadyLinks::new(),
            deadline: DeadlineEntity::new(),
        }
    }

    /// Returns a reference to the inner task struct.
    pub const fn inner(&self) -> &T {
        &self.inner
    }

    /// Returns committed, unweighted execution time in nanoseconds.
    ///
    /// May be read on another CPU without the run-queue lock. Pending time on
    /// a running task is not refreshed; this value does not publish task state.
    /// Idle tasks remain at zero. The counter saturates rather than wrapping.
    pub fn sum_exec_runtime_ns(&self) -> u64 {
        self.sum_exec_runtime_ns.load(Ordering::Relaxed)
    }

    /// Commits an interval consumed by the owning scheduler's `charge` path.
    ///
    /// The caller must hold this task's owning RQ lock and account only its
    /// current task. `RqLocked` does not encode which RQ is locked. Migration
    /// must finish source accounting and detach the task before the destination
    /// can account it. Remote readers and exit-statistics consumers must not write.
    /// Every write must preserve this single-writer ownership: the load/store
    /// below is not an atomic RMW. Adding an independently synchronized writer
    /// requires redesigning interval ownership, not just making the sum atomic.
    pub(crate) fn account_runtime(&self, elapsed_ns: u64, _tok: RqLocked) {
        let total = self.sum_exec_runtime_ns.load(Ordering::Relaxed);
        self.sum_exec_runtime_ns
            .store(total.saturating_add(elapsed_ns), Ordering::Relaxed);
    }

    /// Live class. Acquire so a lock-free CPU-selection read sees the last
    /// Release store from `set_class`.
    pub fn sched_class(&self) -> SchedClass {
        SchedClass::from_u8(self.class.load(Ordering::Acquire))
    }

    /// Live FIFO/RR priority, or `0` when this task is not RT.
    ///
    /// Acquire pairs with the Release store in [`Self::set_attr`] /
    /// [`Self::init_attr_unpublished`] so CPU selection can compare against a
    /// remote run queue without that scheduler lock.
    pub fn rt_priority_snapshot(&self) -> u8 {
        self.rt_prio.load(Ordering::Acquire)
    }

    /// `true` when this task is the per-CPU idle class.
    pub fn is_idle_class(&self) -> bool {
        self.sched_class() == SchedClass::Idle
    }

    /// Write attr and class on a task that has not been published yet.
    ///
    /// # Errors
    ///
    /// Returns the first [`SchedAttr::validate`] failure.
    #[expect(
        dead_code,
        reason = "CG-024-lint-exceptions: dead_code: ktask does not publish a non-Fair SchedAttr \
                  until sched_* calls set_attr"
    )]
    pub(crate) fn init_attr_unpublished(&self, attr: SchedAttr) -> Result<(), SchedError> {
        attr.validate()?;
        // SAFETY: the creator still exclusively owns this unpublished task;
        // it is not on any run queue and no other CPU can observe it.
        let tok = unsafe { RqLocked::assume() };
        self.attr.set(attr, tok);
        self.fair.weight.set(attr.nice.weight(), tok);
        self.store_rt_prio(attr);
        self.class.store(attr.class().as_u8(), Ordering::Release);
        Ok(())
    }

    pub(crate) fn attr(&self, tok: RqLocked) -> SchedAttr {
        self.attr.get(tok)
    }

    /// Store `attr` and the nice weight Fair uses for vruntime.
    ///
    /// Fair rescale reads [`FairEntity::weight`], so this must run before
    /// `attr_changed`.
    pub(crate) fn set_attr(&self, attr: SchedAttr, tok: RqLocked) {
        self.attr.set(attr, tok);
        self.fair.weight.set(attr.nice.weight(), tok);
        self.store_rt_prio(attr);
    }

    fn store_rt_prio(&self, attr: SchedAttr) {
        let prio = if attr.policy.is_realtime() {
            attr.rt_priority.get()
        } else {
            0
        };
        self.rt_prio.store(prio, Ordering::Release);
    }

    pub(crate) fn set_class(&self, class: SchedClass, _tok: RqLocked) {
        self.class.store(class.as_u8(), Ordering::Release);
    }

    pub(crate) fn seat(&self, tok: RqLocked) -> QueueSeat {
        self.seat.get(tok)
    }

    pub(crate) fn set_seat(&self, seat: QueueSeat, tok: RqLocked) {
        self.seat.set(seat, tok);
    }

    pub(crate) fn fair(&self) -> &FairEntity {
        &self.fair
    }

    /// CBS/EDF state. Only the Deadline class reads or writes it.
    pub(crate) const fn deadline(&self) -> &DeadlineEntity {
        &self.deadline
    }

    pub(crate) fn rt(&self) -> &RtEntity {
        &self.rt
    }

    pub(crate) fn policy(&self, tok: RqLocked) -> SchedPolicy {
        self.attr.get(tok).policy
    }

    pub(crate) fn rt_priority(&self, tok: RqLocked) -> RtPrio {
        self.attr.get(tok).rt_priority
    }
}

// SAFETY: `OFFSET` is the private `rt_links` field. Only `ReadyList` reads
// that link and its owner word, and only while the owning RQ lock is held.
unsafe impl<T> ReadyItem for SchedTask<T> {
    const OFFSET: usize = core::mem::offset_of!(Self, rt_links);

    fn ready_links(&self) -> &ReadyLinks {
        &self.rt_links
    }
}

#[cfg(unittest)]
impl<T> SchedTask<T> {
    pub(crate) fn test_token() -> RqLocked {
        // SAFETY: kernel unittests run on one thread, so this read or write
        // cannot interleave with a `Scheduler` update of the same cell.
        unsafe { RqLocked::assume() }
    }

    pub(crate) fn vruntime_for_test(&self) -> i64 {
        self.fair.vruntime.get(Self::test_token())
    }

    pub(crate) fn set_vruntime_for_test(&self, v: i64) {
        self.fair.vruntime.set(v, Self::test_token());
    }

    pub(crate) fn vlag_for_test(&self) -> i64 {
        self.fair.vlag.get(Self::test_token())
    }

    pub(crate) fn set_vlag_for_test(&self, lag: i64) {
        self.fair.vlag.set(lag, Self::test_token());
    }

    pub(crate) fn deadline_for_test(&self) -> i64 {
        self.fair.deadline.get(Self::test_token())
    }

    pub(crate) fn set_deadline_for_test(&self, d: i64) {
        self.fair.deadline.set(d, Self::test_token());
    }

    pub(crate) fn slice_for_test(&self) -> u64 {
        self.fair.slice_left_ns.get(Self::test_token())
    }

    pub(crate) fn set_request_ns_for_test(&self, ns: u64) -> bool {
        if ns == 0 {
            return false;
        }
        let tok = Self::test_token();
        self.fair.request_ns.set(ns, tok);
        self.fair.slice_left_ns.set(ns, tok);
        true
    }

    pub(crate) fn reset_slice_for_test(&self) {
        let tok = Self::test_token();
        let request = self.fair.request_ns.get(tok);
        self.fair.slice_left_ns.set(request, tok);
    }

    pub(crate) fn request_ns_for_test(&self) -> u64 {
        self.fair.request_ns.get(Self::test_token())
    }

    /// Remaining CBS budget, for deadline class assertions.
    pub(crate) fn dl_budget_for_test(&self) -> u64 {
        self.deadline.budget_left_ns.get(Self::test_token())
    }

    /// Whether the task is waiting for its next period.
    pub(crate) fn dl_is_throttled_for_test(&self) -> bool {
        self.deadline.is_throttled.get(Self::test_token())
    }

    /// Absolute instant the next period starts.
    pub(crate) fn dl_period_start_for_test(&self) -> u64 {
        self.deadline.period_start_ns.get(Self::test_token())
    }
}

impl<T> Deref for SchedTask<T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

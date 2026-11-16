// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `dma_fence`: one completion point that several independent owners observe.
//!
//! A fence starts pending, reaches a terminal state exactly once, and keeps the
//! error it reached that state with (zero for success). Waiting is expressed
//! through Linux's callback list: everybody who needs to react to the fence
//! registers a [`FenceCallback`] while it is pending, and the signal walks that
//! list. The two callback kinds are the two things a fence can mean to another
//! party:
//!
//! * [`FenceCallback::MemberDone`] / [`FenceCallback::ChainPremise`] /
//!   [`FenceCallback::ChainWork`] — "when this fence completes, tell *that*
//!   collection". A fence array's head and a chain node complete once their
//!   dependencies did, without anyone polling.
//! * [`FenceCallback::Wake`] — "when this fence completes, wake this waiter".
//!   This is the parked-task path of `SYNCOBJ_WAIT`, `read()` and `poll()` on a
//!   sync_file or dma-buf.
//!
//! Callbacks run in the context that completed the fence — possibly an
//! interrupt handler — after the fence's lock is released, so a chain may
//! complete another fence that depends back on this one. They must be short and
//! non-blocking; waking a task only enqueues it.
//!
//! The sequence numbers of one [`SyncContext`] give a total order. Fences that
//! do not belong to a driver context use a context of their own and a global
//! counter, so every fence userspace can observe has a unique
//! `(context, seqno)` pair.

use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};

use kspin::SpinNoIrq;
use ktime_types::MonotonicInstant;

use super::fence_list::{FenceList, first_error, is_signaled};

/// A fence that has not been signaled yet.
const STATE_PENDING: u8 = 0;
/// A fence whose work completed successfully.
const STATE_SIGNALED: u8 = 1;
/// A fence whose work failed or was canceled.
const STATE_ERROR: u8 = 2;

/// The namespace a fence's sequence number belongs to.
///
/// Fences of one context are totally ordered by sequence number; fences of
/// different contexts are only ordered by their signaling. Distinct drivers and
/// the anonymous source therefore use distinct values.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(crate) struct SyncContext(u64);

impl SyncContext {
    /// A driver-owned context.
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The numeric context reported to userspace.
    pub(crate) const fn value(self) -> u64 {
        self.0
    }
}

/// Context of fences created by `new_anonymous_fence`.
const ANONYMOUS_CONTEXT: SyncContext = SyncContext(0);
/// Context of fences created by [`new_signaled_fence`].
const PRIVATE_CONTEXT: SyncContext = SyncContext(2);

static ANONYMOUS_SEQNO: AtomicU64 = AtomicU64::new(0);
static PRIVATE_SEQNO: AtomicU64 = AtomicU64::new(0);

/// Creates a pending fence with no driver, returning the capability that
/// completes it.
///
/// A display commit uses this to express "this flip is not on screen yet": the
/// card keeps the capability and runs it on the completion point it can
/// justify, which is also the point at which an `OUT_FENCE_PTR` descriptor
/// becomes readable.
#[cfg(unittest)]
pub(crate) fn new_anonymous_fence() -> (Arc<DmaFence>, Arc<FenceSignal>) {
    new_anonymous_fence_named("unknown")
}

/// Creates a pending fence attributed to a concrete driver.
pub(crate) fn new_anonymous_fence_named(
    driver_name: &'static str,
) -> (Arc<DmaFence>, Arc<FenceSignal>) {
    let seqno = ANONYMOUS_SEQNO.fetch_add(1, Ordering::Relaxed);
    DmaFence::new_owned_named(ANONYMOUS_CONTEXT, seqno, Some(driver_name))
}

/// Creates a signaled fence in the private context.
pub(crate) fn new_signaled_fence() -> Arc<DmaFence> {
    let seqno = PRIVATE_SEQNO.fetch_add(1, Ordering::Relaxed);
    DmaFence::new_signaled(PRIVATE_CONTEXT, seqno)
}

/// One completion point.
pub(crate) struct DmaFence {
    context: SyncContext,
    seqno: u64,
    /// Driver that produced this fence, when it is a driver fence.
    driver_name: Option<&'static str>,
    /// Output timestamp, recorded when the fence completes.
    timestamp: SpinNoIrq<Option<Timestamp>>,
    /// Callbacks instead of Linux's `cb_list`, see the module documentation.
    callbacks: SpinNoIrq<Vec<FenceCallback>>,
    /// Terminal state; `STATE_PENDING` until it changes once.
    state: AtomicU8,
    /// The error the fence finished with.
    error: AtomicU32,
    /// The set this fence merges, when it is a fence array's head.
    ///
    /// Linux recovers the same information from the fence's ops; keeping it on
    /// the fence lets `sync_file_info` list the members without a downcast.
    array: SpinNoIrq<Option<Arc<super::fence_array::FenceArray>>>,
    /// The premise this fence chains onto, when it is a chain node.
    ///
    /// Linux's `dma_fence_chain::prev`, held as the same atomic-swappable
    /// cell so a later link walks past the completed nodes.
    prev: SpinNoIrq<Option<Arc<DmaFence>>>,
    /// The work this chain node waits for (`dma_fence_chain::fence`).
    contained: SpinNoIrq<Option<Arc<DmaFence>>>,
    /// The deadline the producer published for this fence (`set_deadline`).
    ///
    /// It is a hint for a waiter that would otherwise wait unbounded; it never
    /// completes the fence.
    deadline: SpinNoIrq<Option<MonotonicInstant>>,
}

impl DmaFence {
    /// Creates a pending fence together with the only capability that can
    /// complete it.
    pub(crate) fn new_owned(context: SyncContext, seqno: u64) -> (Arc<Self>, Arc<FenceSignal>) {
        Self::new_owned_named(context, seqno, None)
    }

    pub(crate) fn new_owned_named(
        context: SyncContext,
        seqno: u64,
        driver_name: Option<&'static str>,
    ) -> (Arc<Self>, Arc<FenceSignal>) {
        let fence = Arc::new(Self::new(context, seqno, driver_name));
        let signal = Arc::new(FenceSignal {
            fence: fence.clone(),
        });
        (fence, signal)
    }

    /// Creates an already signaled fence.
    pub(crate) fn new_signaled(context: SyncContext, seqno: u64) -> Arc<Self> {
        let fence = Self::new(context, seqno, None);
        *fence.timestamp.lock() = Some(Timestamp(khal::time::monotonic_time()));
        fence.state.store(STATE_SIGNALED, Ordering::Release);
        Arc::new(fence)
    }

    fn new(context: SyncContext, seqno: u64, driver_name: Option<&'static str>) -> Self {
        Self {
            context,
            seqno,
            driver_name,
            timestamp: SpinNoIrq::new(None),
            callbacks: SpinNoIrq::new(Vec::new()),
            state: AtomicU8::new(STATE_PENDING),
            error: AtomicU32::new(0),
            array: SpinNoIrq::new(None),
            prev: SpinNoIrq::new(None),
            contained: SpinNoIrq::new(None),
            deadline: SpinNoIrq::new(None),
        }
    }

    /// The driver name this fence reports, when it is a driver fence.
    pub(crate) const fn driver_name(&self) -> Option<&'static str> {
        self.driver_name
    }

    /// Creates a chain node that completes when `prev` and `fence` did.
    ///
    /// This is `dma_fence_chain_init`: the node completes after its premise
    /// (`prev`) and after the work it contains (`fence`), so a chain records
    /// an ordered dependency without waiting for anything to be signaled at
    /// construction time. `seqno` orders nodes of one timeline.
    pub(crate) fn new_chain(
        prev: Option<Arc<DmaFence>>,
        fence: Arc<DmaFence>,
        context: SyncContext,
        seqno: u64,
    ) -> (Arc<Self>, Arc<FenceSignal>) {
        let (node, signal) = Self::new_owned(context, seqno);
        *node.prev.lock() = prev;
        *node.contained.lock() = Some(fence);
        (node, signal)
    }

    /// The premise of a chain node, if the node still holds one.
    pub(crate) fn chain_prev(&self) -> Option<Arc<DmaFence>> {
        self.prev.lock().clone()
    }

    /// Replaces the premise of a chain node with `next`.
    ///
    /// While the node waits, its premise advances past every node that already
    /// completed, so a later wait on the same chain starts at the work that is
    /// still outstanding instead of the node it was created with.
    pub(crate) fn set_chain_prev(&self, next: Option<Arc<DmaFence>>) {
        *self.prev.lock() = next;
    }

    /// The work contained by a chain node.
    pub(crate) fn contained(&self) -> Option<Arc<DmaFence>> {
        self.contained.lock().clone()
    }

    /// Whether this fence is a chain node.
    pub(crate) fn is_chain(&self) -> bool {
        self.contained.lock().is_some()
    }

    /// The instant a producer asked this fence to be finished by.
    pub(crate) fn deadline(&self) -> Option<MonotonicInstant> {
        *self.deadline.lock()
    }

    /// Publishes a deadline hint for this fence (`dma_fence_set_deadline`).
    ///
    /// A chain node forwards the hint to its contained work, as Linux's chain
    /// ops do, so the producer that knows the queue's own deadline can bound
    /// the wait of its consumers without holding a timer themselves.
    pub(crate) fn set_deadline(&self, deadline: MonotonicInstant) {
        *self.deadline.lock() = Some(deadline);
        if let Some(contained) = self.contained() {
            contained.set_deadline(deadline);
        }
    }

    /// The fence's sequence number in its context.
    pub(crate) const fn seqno(&self) -> u64 {
        self.seqno
    }

    /// The fence's context.
    pub(crate) const fn context(&self) -> SyncContext {
        self.context
    }

    /// Whether the fence reached a terminal state.
    pub(crate) fn is_signaled(&self) -> bool {
        self.state.load(Ordering::Acquire) != STATE_PENDING
    }

    /// The error the fence finished with; zero while pending or successful.
    ///
    /// A chain node's state is what its premise and its work produced, so the
    /// terminal state is kept in sync with both and never reported early.
    pub(crate) fn error(&self) -> i32 {
        if self.is_signaled() {
            self.error.load(Ordering::Acquire) as i32
        } else {
            0
        }
    }

    /// The array this fence is the head of, if the array still exists.
    ///
    /// The head holds the array weakly and the array holds the head strongly,
    /// so a head that outlives its array (a client kept the syncobj alive but
    /// the merge was destroyed) is an ordinary fence again.
    pub(crate) fn array(&self) -> Option<Arc<super::fence_array::FenceArray>> {
        self.array.lock().clone()
    }

    /// Records `array` as the set this fence merges.
    ///
    /// Called once by the array constructor before the head is published.
    pub(crate) fn set_array(&self, array: &Arc<super::fence_array::FenceArray>) {
        *self.array.lock() = Some(array.clone());
    }

    /// The instant the fence completed, if it did.
    pub(crate) fn timestamp(&self) -> Option<Timestamp> {
        *self.timestamp.lock()
    }

    /// Completes the premise upon which this fence's timestamp rests, without
    /// taking its own completion.
    ///
    /// A fence array's head completes with the member that finished last, so it
    /// reports that member's instant instead of the instant the array happened
    /// to notice. Only the first value counts, as `dma_fence_set_timestamp`
    /// specifies.
    pub(crate) fn inherit_timestamp(&self, from: Timestamp) {
        let mut slot = self.timestamp.lock();
        if slot.is_none() {
            *slot = Some(from);
        }
    }

    /// Registers a callback for the terminal state, or runs it at once (in the
    /// caller's context) when the fence already reached one.
    ///
    /// Holds only the callback-list lock and never sleeps, so it is valid in
    /// any context that may take a spinlock. The fence's own lock is not held
    /// while the callback runs.
    pub(crate) fn add_callback(&self, callback: FenceCallback) {
        let mut callbacks = self.callbacks.lock();
        let error = match self.state.load(Ordering::Acquire) {
            STATE_PENDING => {
                callbacks.push(callback);
                return;
            }
            STATE_SIGNALED => 0,
            _ => self.error.load(Ordering::Acquire) as i32,
        };
        drop(callbacks);
        dispatch(callback, error);
    }

    /// Marks the work successful and returns the number of callbacks run.
    ///
    /// A fence signals once; a second call runs nothing and reports zero, which
    /// lets a user tell "I completed it" from "somebody else did".
    pub(crate) fn signal(&self) -> usize {
        self.finish(STATE_SIGNALED, 0)
    }

    /// Marks the work failed and returns the number of callbacks run.
    pub(crate) fn signal_error(&self, error: i32) -> usize {
        self.finish(STATE_ERROR, error)
    }

    /// Moves a pending fence to `terminal` and runs its callbacks once.
    ///
    /// The callback-list lock is the single point through which a fence changes
    /// state, so a registration cannot observe "pending" after a signal already
    /// drained the list.
    fn finish(&self, terminal: u8, error: i32) -> usize {
        let callbacks = {
            let mut pending = self.callbacks.lock();
            if self.state.load(Ordering::Acquire) != STATE_PENDING {
                return 0;
            }
            self.error.store(error as u32, Ordering::Release);
            // The timestamp is the completion instant, recorded before the
            // state that publishes it.
            *self.timestamp.lock() = Some(Timestamp(khal::time::monotonic_time()));
            self.state.store(terminal, Ordering::Release);
            core::mem::take(&mut *pending)
        };
        let count = callbacks.len();
        // The terminal state and the error are published before any callback
        // runs, so a callback that re-reads the fence sees its outcome.
        let error = self.error.load(Ordering::Acquire) as i32;
        for callback in callbacks {
            dispatch(callback, error);
        }
        count
    }
}

impl core::fmt::Debug for DmaFence {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DmaFence")
            .field("context", &self.context.value())
            .field("seqno", &self.seqno)
            .field("error", &self.error())
            .finish()
    }
}

/// The output timestamp of a fence.
///
/// Linux's `sync_fence_info::timestamp_ns` is the instant the fence completed,
/// so this is recorded once in [`DmaFence::finish`] rather than by each user.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Timestamp(MonotonicInstant);

impl Timestamp {
    /// Nanoseconds since the monotonic origin.
    ///
    /// This is `ktime_to_ns(dma_fence_timestamp(fence))`; the observer
    /// argument names the instant the observer obtained the fence, which only
    /// matters for a timestamp that was recorded as an offset.
    pub(crate) fn nanos(&self, _observer: MonotonicInstant) -> u64 {
        self.0.as_nanos_u64_saturating()
    }
}

/// The only capability to complete one fence.
///
/// It is `Send + Sync` so a driver can hand it to an interrupt handler. It can
/// only move its fence forward, and dropping it without a signal completes the
/// fence with an error instead of leaving waiters forever.
pub(crate) struct FenceSignal {
    fence: Arc<DmaFence>,
}

impl FenceSignal {
    /// Completes the work the fence describes.
    pub(crate) fn signal(&self) -> usize {
        self.fence.signal()
    }

    /// Completes the work with an error.
    pub(crate) fn signal_error(&self, error: i32) -> usize {
        self.fence.signal_error(error)
    }

    /// The fence this capability completes.
    pub(crate) fn fence(&self) -> &Arc<DmaFence> {
        &self.fence
    }
}

impl core::fmt::Debug for FenceSignal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("FenceSignal").field(&self.fence).finish()
    }
}

/// What a fence does when it completes.
///
/// Every variant runs in the context that completed the fence and must be short
/// and non-blocking.
pub(crate) enum FenceCallback {
    /// Wake a task waiting on a [`Waiter`].
    Wake(Weak<Waiter>),
    /// Wake the poll waiters of an object whose readiness is this fence.
    Wakeable(Weak<super::wakeable::Wakeable>),
    /// Report to a fence array that one of its members completed.
    MemberDone(Weak<super::fence_array::FenceArray>),
    /// Tell a chain node that its premise completed.
    ChainPremise(Weak<DmaFence>),
    /// Tell a chain node that its contained work completed.
    ChainWork(Weak<DmaFence>),
    /// Signal the eventfd a `syncobj` event registration watches.
    EventFd(Weak<crate::model::syncobj_signal::EventFdSignal>),
    /// Wake one direction of a dma-buf descriptor's poll waiters.
    DmaBufWake(Weak<super::wakeable::Wakeable>, bool),
}

impl FenceCallback {
    /// A callback that wakes `waiter`, or does nothing once it is gone.
    pub(crate) fn wake(waiter: &Weak<Waiter>) -> Self {
        Self::Wake(waiter.clone())
    }

    /// A callback that wakes the waiters of `source`.
    pub(crate) fn wakeable(source: &Weak<super::wakeable::Wakeable>) -> Self {
        Self::Wakeable(source.clone())
    }

    /// A callback that reports one of `array`'s members as completed.
    pub(crate) fn member_done(array: &Arc<super::fence_array::FenceArray>) -> Self {
        Self::MemberDone(Arc::downgrade(array))
    }

    /// A callback that tells `node` its premise completed.
    pub(crate) fn chain_premise(node: &Arc<DmaFence>) -> Self {
        Self::ChainPremise(Arc::downgrade(node))
    }

    /// A callback that tells `node` its contained work completed.
    pub(crate) fn chain_work(node: &Arc<DmaFence>) -> Self {
        Self::ChainWork(Arc::downgrade(node))
    }

    /// A callback that signals an eventfd registration.
    pub(crate) fn eventfd(event: &Weak<crate::model::syncobj_signal::EventFdSignal>) -> Self {
        Self::EventFd(event.clone())
    }

    /// A callback that wakes a dma-buf descriptor's waiters for one direction.
    pub(crate) fn dma_buf_wakeable(source: &Weak<super::wakeable::Wakeable>, write: bool) -> Self {
        Self::DmaBufWake(source.clone(), write)
    }
}

fn dispatch(callback: FenceCallback, _error: i32) {
    match callback {
        FenceCallback::Wake(waiter) => {
            if let Some(waiter) = waiter.upgrade() {
                waiter.wake();
            }
        }
        FenceCallback::Wakeable(source) => {
            if let Some(source) = source.upgrade() {
                source.notify();
            }
        }
        FenceCallback::MemberDone(array) => {
            if let Some(array) = array.upgrade() {
                array.try_signal();
            }
        }
        FenceCallback::ChainPremise(node) | FenceCallback::ChainWork(node) => {
            if let Some(node) = node.upgrade() {
                super::fence_chain::try_complete(&node);
            }
        }
        FenceCallback::EventFd(event) => {
            if let Some(event) = event.upgrade() {
                event.signal();
            }
        }
        FenceCallback::DmaBufWake(source, write) => {
            if let Some(source) = source.upgrade() {
                source.wake_direction(write);
            }
        }
    }
}

/// The task side of a wait that is not decided yet.
///
/// A waiter is owned by the waiting call for its whole duration; the fences it
/// watches only hold weak references, so a fence that outlives the wait cannot
/// keep the task reachable.
pub(crate) struct Waiter {
    /// The task's waker, installed before the task parks.
    waker: SpinNoIrq<Option<core::task::Waker>>,
    /// Fences this waiter registered a wake callback with, so a re-park does not
    /// accumulate callbacks.
    watched: SpinNoIrq<Vec<Weak<DmaFence>>>,
}

impl Waiter {
    fn new() -> Self {
        Self {
            waker: SpinNoIrq::new(None),
            watched: SpinNoIrq::new(Vec::new()),
        }
    }

    /// Wakes the parked task once.
    ///
    /// Used by the structure a wait observes when its state changes without a
    /// watched fence completing.
    pub(crate) fn poke(waiter: &Waiter) {
        waiter.wake();
    }

    /// Wakes the parked task once.
    fn wake(&self) {
        let waker = self.waker.lock().take();
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// Registers one wake callback on every pending fence of `list` that this
    /// waiter does not watch yet.
    fn watch(&self, list: &FenceList, handle: &Arc<Waiter>) {
        let mut to_register = Vec::new();
        {
            let mut watched = self.watched.lock();
            watched.retain(|known| known.strong_count() != 0);
            for fence in list.fences() {
                let weak = Arc::downgrade(fence);
                if fence.is_signaled() || watched.iter().any(|known| known.ptr_eq(&weak)) {
                    continue;
                }
                watched.push(weak);
                to_register.push(fence.clone());
            }
        }
        // Callbacks are added after the watch-list lock is released, so the
        // fence lock is never taken while this lock is held.
        for fence in to_register {
            fence.add_callback(FenceCallback::wake(&Arc::downgrade(handle)));
        }
    }
}

/// The task-side handle of one wait.
///
/// The caller owns this value for the whole wait and re-evaluates after every
/// [`Wait::outcome`] that returns `None`.
pub(crate) struct Wait {
    /// The waiter the fences hold weakly.
    waiter: Arc<Waiter>,
}

impl Wait {
    /// Creates the task-side handle of one wait.
    pub(crate) fn new() -> Self {
        Self {
            waiter: Arc::new(Waiter::new()),
        }
    }

    /// The outcome the wait should report, or `None` when the task must park.
    ///
    /// Returns `Some(0)` when every fence of `list` succeeded and
    /// `Some(errno)` when one failed. When it returns `None`, the waker is
    /// installed and the waiter subscribed to `list`, so a later completion is
    /// reported through a fence callback or [`Wait::poke`].
    pub(crate) fn outcome(&self, list: &FenceList, waker: &core::task::Waker) -> Option<i32> {
        let error = first_error(list);
        if error != 0 {
            return Some(error);
        }
        if is_signaled(list) {
            return Some(0);
        }
        *self.waiter.waker.lock() = Some(waker.clone());
        self.waiter.watch(list, &self.waiter);
        // Re-check after the installation: a fence that completed in between
        // was not seen by a wake callback, so the answer is read again.
        let error = first_error(list);
        if error != 0 {
            return Some(error);
        }
        if is_signaled(list) {
            return Some(0);
        }
        None
    }

    /// The waiter, for a structure that keeps only a weak reference to it.
    pub(crate) fn waiter(&self) -> &Arc<Waiter> {
        &self.waiter
    }
}

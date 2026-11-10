// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `syncobj`: client-owned fence containers (`drm_syncobj.c`).
//!
//! A syncobj is an object userspace names through a per-file handle, like a
//! GEM handle or a framebuffer id. Its value is either a single fence (the
//! binary form) or a timeline: a map from point numbers to fences, where every
//! point up to and including an available one is available.
//!
//! ## Replacement is a dependency
//!
//! Setting an object's value chains the new fence onto the value it replaces:
//! the new value is signaled only after the old one completed. That single rule
//! gives several commands their Linux meaning:
//!
//! * `FD_TO_HANDLE` records "after the imported fence and after whatever this
//!   object already held", so importing twice cannot lose the earlier import;
//! * `TIMELINE_SIGNAL` of a point waits for the value the point held before,
//!   which is how a producer keeps a consumer's earlier dependency visible.
//!
//! ## Publication
//!
//! The value sits behind one IRQ-safe lock. A replacement publishes the new
//! value first and only then chains the old fence onto it, so a waiter that
//! re-reads the value after any change sees either the old value (still
//! pending, so the chain callback will arrive) or the new one. A replacement
//! also pokes every waiter parked on the old value: that value was replaced and
//! may never complete, and only a re-read can observe the new one.

use alloc::{
    collections::BTreeMap,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};

use kspin::SpinNoIrq;
use ktime_types::MonotonicInstant;

use super::{
    error::{DrmError, DrmResult},
    ids::{Id, IdRegistry, SyncobjTag},
};
use crate::sync::{
    dma_fence::{DmaFence, Wait, Waiter, new_signaled_fence},
    fence_list::FenceList,
};

/// A syncobj's userspace-visible name in one open file.
pub(crate) type SyncobjHandle = Id<SyncobjTag>;

/// A timeline: the fence that made each point available.
#[derive(Default)]
struct Timeline {
    /// Point -> fence. A wait for `p` observes the fence of the closest point
    /// at or below `p`.
    fences: BTreeMap<u64, Arc<DmaFence>>,
}

impl Timeline {
    /// The fence that makes `point` available, if a point covers it.
    ///
    /// A timeline point is satisfied by the fence of that point **or of any
    /// later one**: the points form one ordered sequence, so completing point 7
    /// also completes 3. The closest point at or above `point` is the one that
    /// observes the dependency the client asked about, which is what
    /// `dma_fence_chain_find_seqno` walks the chain for.
    fn fence(&self, point: u64) -> Option<Arc<DmaFence>> {
        self.fences
            .range(point..)
            .next()
            .map(|(_, fence)| fence.clone())
            .or_else(|| {
                // A signal for an earlier point that has not been replaced by a
                // later one still satisfies nothing at or above `point`, so the
                // wait must not observe it.
                None
            })
    }

    /// Records `fence` as the value of `point`.
    fn set(&mut self, point: u64, fence: Arc<DmaFence>) {
        self.fences.insert(point, fence);
    }

    /// The highest point whose fence completed.
    fn available_point(&self) -> u64 {
        self.fences
            .iter()
            .rev()
            .find(|(_, fence)| fence.is_signaled())
            .map_or(0, |(point, _)| *point)
    }
}

/// The value of one syncobj.
enum Value {
    /// The binary form: one fence, or no value at all.
    Binary(Option<Arc<DmaFence>>),
    /// The timeline form.
    Timeline(Timeline),
}

/// One syncobj.
pub(crate) struct Syncobj {
    /// The current value.
    value: SpinNoIrq<Value>,
    /// Waiters parked on the current value.
    waiters: SpinNoIrq<Vec<Weak<Waiter>>>,
    /// Eventfds registered for this object's timeline points
    /// (`drm_syncobj::syncobj_event_fence` relationships).
    events: SpinNoIrq<Vec<Arc<super::syncobj_signal::EventFdSignal>>>,
    /// Name reported in `sync_file_info` of an exported descriptor.
    pub(crate) name: String,
}

impl Syncobj {
    /// Creates an empty binary object.
    pub(crate) fn new_binary() -> Arc<Self> {
        Arc::new(Self {
            value: SpinNoIrq::new(Value::Binary(None)),
            waiters: SpinNoIrq::new(Vec::new()),
            events: SpinNoIrq::new(Vec::new()),
            name: String::from("syncobj"),
        })
    }

    /// The binary value's fence.
    pub(crate) fn binary_fence(&self) -> Option<Arc<DmaFence>> {
        match &*self.value.lock() {
            Value::Binary(fence) => fence.clone(),
            Value::Timeline(_) => None,
        }
    }

    /// The fence that makes timeline `point` available.
    pub(crate) fn timeline_fence(&self, point: u64) -> Option<Arc<DmaFence>> {
        match &*self.value.lock() {
            Value::Binary(_) => None,
            Value::Timeline(timeline) => timeline.fence(point),
        }
    }

    /// The highest timeline point whose fence completed, or zero.
    pub(crate) fn available_point(&self) -> u64 {
        match &*self.value.lock() {
            Value::Timeline(timeline) => timeline.available_point(),
            Value::Binary(fence) => {
                u64::from(fence.as_ref().is_some_and(|fence| fence.is_signaled()))
            }
        }
    }

    /// Installs `fence` as the binary value of a new generation.
    ///
    /// This is `drm_syncobj_replace_fence`: `SIGNAL`, `RESET` and
    /// `FD_TO_HANDLE` replace the object's fence outright. Linux does not
    /// chain a binary replacement, so the previous fence stops being part of
    /// the object; only a version counter is bumped and the waiters and
    /// eventfds are re-evaluated.
    pub(crate) fn replace_binary(&self, fence: Arc<DmaFence>) {
        self.install_binary(fence, None);
    }

    /// Installs `fence` as the binary value over `previous`.
    fn install_binary(&self, fence: Arc<DmaFence>, previous: Option<Arc<DmaFence>>) {
        let mut value = self.value.lock();
        let installed = installed_value(fence, previous);
        *value = Value::Binary(Some(installed.clone()));
        let waiters = self.take_waiters();
        drop(value);
        self.rearm_events(&installed);
        poke(waiters);
        // A value that replaces the object's state is also what signals the
        // eventfds registered on it, as `drm_syncobj_signal_ioctl` does when
        // the new value makes the point available.
        self.signal_events(&installed);
    }

    /// Marks the binary value available with a fence that completes at once.
    pub(crate) fn signal_binary(&self) {
        self.replace_binary(new_signaled_fence());
    }

    /// Clears the object's value.
    ///
    /// A binary object loses its fence; a timeline loses every point, so a
    /// wait for any point that was signaled reports "not available" again,
    /// exactly as `drm_syncobj_reset_ioctl` resets `syncobj->timeline` points.
    pub(crate) fn reset(&self) {
        let mut value = self.value.lock();
        // Every value form is clearable, so the lock is taken for all of them
        // and the waiters are poked after it is released.
        *value = match &*value {
            Value::Timeline(_) => Value::Timeline(Timeline::default()),
            Value::Binary(_) => Value::Binary(None),
        };
        let waiters = self.take_waiters();
        drop(value);
        poke(waiters);
    }

    /// Installs `fence` as the value of timeline `point`.
    ///
    /// A binary object becomes a timeline whose only point is `point`. The new
    /// point becomes a chain node over the value the point held before, so a
    /// producer keeps the earlier dependency of the point visible.
    pub(crate) fn set_timeline(&self, point: u64, fence: Arc<DmaFence>) {
        let mut value = self.value.lock();
        let previous = match &mut *value {
            Value::Timeline(timeline) => timeline.fence(point),
            Value::Binary(previous) => previous.clone(),
        };
        let installed = installed_value(fence, previous);
        match &mut *value {
            Value::Timeline(timeline) => timeline.set(point, installed.clone()),
            Value::Binary(_) => {
                let mut timeline = Timeline::default();
                timeline.set(point, installed.clone());
                *value = Value::Timeline(timeline);
            }
        }
        let waiters = self.take_waiters();
        drop(value);
        self.rearm_events(&installed);
        self.signal_events(&installed);
        poke(waiters);
    }

    /// Registers an event that fires on this object's timeline point.
    ///
    /// The event is armed on the value that will make the point available. A
    /// point that a later value makes available again signals the same
    /// relation once more, because the next value is a new instance the event
    /// arms on when it is installed.
    pub(crate) fn add_event(&self, event: &Arc<super::syncobj_signal::EventFdSignal>) {
        {
            let mut events = self.events.lock();
            if events.iter().any(|known| Arc::ptr_eq(known, event)) {
                return;
            }
            events.push(event.clone());
        }
        // The value that will make the point available is the one currently
        // installed. A replacement installs a new value; when that value is
        // installed, `re-arm` arms this event on it so the eventfd keeps
        // counting points instead of firing once.
        if let Some(value) = self.value_for_point(event.point) {
            event.arm(&value);
        }
        event.signal_on_install();
    }

    /// The installed value that makes `point` available, if one is installed.
    fn value_for_point(&self, point: u64) -> Option<Arc<DmaFence>> {
        match &*self.value.lock() {
            Value::Binary(fence) => fence.clone(),
            Value::Timeline(timeline) => timeline.fence(point),
        }
    }

    /// Signals every eventfd whose relationship this object holds.
    ///
    /// A signal is counted once per installed value that makes the point
    /// available, and armed for the next one. The count saturates in the
    /// eventfd itself, so a long chain of signals cannot overflow it.
    fn signal_events(&self, value: &Arc<DmaFence>) {
        if value.is_signaled() {
            let events = self.events.lock().clone();
            for event in events {
                event.signal();
            }
        }
    }

    /// Re-arms every registered event on a newly installed value.
    fn rearm_events(&self, value: &Arc<DmaFence>) {
        let events = self.events.lock().clone();
        for event in events {
            event.arm(value);
        }
    }

    /// Registers `waiter` to be poked when the value changes.
    pub(crate) fn watch(&self, waiter: &Arc<Waiter>) {
        let mut waiters = self.waiters.lock();
        waiters.retain(|entry| entry.strong_count() != 0);
        let weak = Arc::downgrade(waiter);
        if waiters.iter().any(|known| known.ptr_eq(&weak)) {
            return;
        }
        waiters.push(weak);
    }

    /// Removes and returns the registered waiters.
    fn take_waiters(&self) -> Vec<Weak<Waiter>> {
        core::mem::take(&mut *self.waiters.lock())
    }
}

/// The value a replacement installs.
///
/// A replacement over a different value becomes a chain node, so it completes
/// only after the value it replaced did; re-installing the value the object
/// already holds is that value, with no node to allocate. This is what keeps
/// a wait bounded and a repeated import cheap at the same time. A value the
/// command replaces wholesale (`SIGNAL`, `RESET`) starts a new generation and
/// passes `None` instead.
fn installed_value(fence: Arc<DmaFence>, previous: Option<Arc<DmaFence>>) -> Arc<DmaFence> {
    let Some(previous) = previous else {
        return fence;
    };
    if Arc::ptr_eq(&previous, &fence) {
        return fence;
    }
    let node = crate::sync::fence_chain::node(Some(previous), fence);
    crate::sync::fence_chain::link(&node, node.chain_prev().as_ref());
    node
}

/// Pokes waiters whose observed value was replaced.
fn poke(waiters: Vec<Weak<Waiter>>) {
    // The waiter's waker is what `Wait::poke` ends up calling; reaching it
    // through the weak reference the object kept is the same wake.
    for waiter in waiters {
        if let Some(waiter) = waiter.upgrade() {
            Waiter::poke(&waiter);
        }
    }
}

/// The syncobjs of one open file (`file_priv->syncobj_table`).
pub(crate) struct SyncobjTable {
    objects: IdRegistry<Arc<Syncobj>, SyncobjTag>,
}

impl SyncobjTable {
    /// An empty table.
    pub(crate) fn new() -> Self {
        Self {
            objects: IdRegistry::new(1),
        }
    }

    /// Inserts a new object and returns its handle.
    pub(crate) fn insert(&mut self, object: Arc<Syncobj>) -> DrmResult<SyncobjHandle> {
        self.objects.insert(object).ok_or(DrmError::NoMemory)
    }

    /// The object behind `handle`.
    pub(crate) fn get(&self, handle: SyncobjHandle) -> DrmResult<Arc<Syncobj>> {
        self.objects.get(handle).cloned().ok_or(DrmError::NotFound)
    }

    /// Removes and returns the object behind `handle`.
    pub(crate) fn remove(&mut self, handle: SyncobjHandle) -> DrmResult<Arc<Syncobj>> {
        self.objects.remove(handle).ok_or(DrmError::NotFound)
    }

    /// The handle that names `object` in this table, if it is there.
    ///
    /// `drm_syncobj_fd_to_handle` re-importing a descriptor this file already
    /// imported finds the same object and must name it by the same handle.
    pub(crate) fn handle_of(&self, object: &Arc<Syncobj>) -> Option<SyncobjHandle> {
        self.objects
            .iter()
            .find_map(|(handle, known)| Arc::ptr_eq(known, object).then_some(handle))
    }
}

impl Default for SyncobjTable {
    fn default() -> Self {
        Self::new()
    }
}

/// What one object offers a wait request.
pub(crate) enum SyncobjOutcome {
    /// The fence that satisfies the request.
    Fence(Arc<DmaFence>),
    /// Nothing covers the request, and no later signal is expected.
    Unset,
}

/// The value a binary wait or a timeline wait for `point` observes.
pub(crate) fn outcome_for(object: &Syncobj, is_timeline: bool, point: u64) -> SyncobjOutcome {
    let fence = if is_timeline {
        object.timeline_fence(point)
    } else {
        // A binary wait on a timeline object observes its current value, which
        // is what a signal through the object descriptor (or any other client)
        // installed: the fence of the furthest point, not "no value".
        object
            .binary_fence()
            .or_else(|| object.timeline_fence(available_or_zero(object)))
    };
    match fence {
        Some(fence) => SyncobjOutcome::Fence(fence),
        None => SyncobjOutcome::Unset,
    }
}

/// The point a binary wait observes on a timeline object.
///
/// `drm_syncobj_fence_get` hands out the object's current fence, which for a
/// timeline is the chain head; asking for the fence at or above the furthest
/// installed point is the same node.
fn available_or_zero(object: &Syncobj) -> u64 {
    match &*object.value.lock() {
        Value::Timeline(timeline) => timeline.fences.keys().next_back().copied().unwrap_or(0),
        Value::Binary(_) => 0,
    }
}

/// How long one wait may stay parked.
///
/// `None` waits forever. A deadline is absolute: `drm_syncobj_wait_ioctl`
/// converts `timeout_nsec` (an absolute CLOCK_MONOTONIC instant) and the wait
/// reports "not available" when it passes, which is a *successful* wait.
#[derive(Clone, Copy)]
pub(crate) enum WaitTimeout {
    /// Wait until the values are available.
    Infinite,
    /// Stop waiting at this instant.
    Until(MonotonicInstant),
}

impl WaitTimeout {
    /// Whether `now` passed the deadline.
    pub(crate) fn is_elapsed(&self, now: MonotonicInstant) -> bool {
        match self {
            Self::Infinite => false,
            Self::Until(deadline) => now >= *deadline,
        }
    }

    /// The deadline, if there is one.
    pub(crate) const fn deadline(&self) -> Option<MonotonicInstant> {
        match self {
            Self::Infinite => None,
            Self::Until(deadline) => Some(*deadline),
        }
    }
}

/// Parks the calling task until object `index` of `objects` is available.
///
/// The object is re-read after every wake, so a value replaced while the task
/// slept is observed instead of the fence that will never complete; a
/// completion of the observed fence wakes the task first, and the deadline
/// bounds the park. A signal and an expired deadline both end the wait
/// successfully, as `drm_syncobj_wait_ioctl` reports them.
pub(crate) fn wait_index(
    objects: &[(Arc<Syncobj>, u64)],
    is_timeline: bool,
    index: usize,
    timeout: WaitTimeout,
) -> DrmResult<()> {
    let Some((object, point)) = objects.get(index) else {
        // An empty wait has nothing to wait for and succeeds.
        return Ok(());
    };
    // Watching the object is what makes a replacement of its value re-read on
    // the next poll; it happens before the first decision so a value that
    // changes right now is not missed.
    let waiter = Wait::new();
    object.watch(waiter.waiter());
    loop {
        let fence = match outcome_for(object, is_timeline, *point) {
            // A signaled value ends the wait; a failed one reports its error.
            SyncobjOutcome::Fence(fence) if fence.is_signaled() => {
                return crate::sync::wait::check_error(&FenceList::new(alloc::vec![fence]));
            }
            SyncobjOutcome::Fence(fence) => fence,
            // Nothing makes an unset value available; a bounded wait ends
            // here, an unbounded one could never end.
            SyncobjOutcome::Unset => {
                return match timeout {
                    WaitTimeout::Infinite => Err(DrmError::InvalidInput),
                    WaitTimeout::Until(_) => Ok(()),
                };
            }
        };
        if timeout.is_elapsed(khal::time::monotonic_time()) {
            return Ok(());
        }
        // A chain node is walked: waiting on its own state would leave the
        // premise it still depends on invisible to the waiter.
        let list = if fence.is_chain() {
            crate::sync::fence_chain::waiting_set(&fence)
        } else {
            FenceList::new(alloc::vec![fence])
        };
        match crate::sync::wait::block_on_with_deadline(
            core::future::poll_fn(|cx| match waiter.outcome(&list, cx.waker()) {
                Some(0) => core::task::Poll::Ready(Ok(())),
                Some(error) => core::task::Poll::Ready(Err(DrmError::from_fence(error))),
                None => core::task::Poll::Pending,
            }),
            timeout.deadline(),
        ) {
            // The runtime's interruptible park was ended by a signal.
            Err(_) => return Ok(()),
            Ok(Ok(())) => {}
            Ok(Err(DrmError::TimedOut)) => return Ok(()),
            Ok(Err(error)) => return Err(error),
        }
    }
}

/// Parks the calling task until the first of `objects` is available.
///
/// Returns the index that satisfied the wait, or `None` when a signal or the
/// deadline ended it before any value was available.
pub(crate) fn wait_first(
    objects: &[(Arc<Syncobj>, u64)],
    is_timeline: bool,
    timeout: WaitTimeout,
) -> DrmResult<Option<u32>> {
    let waiter = Wait::new();
    loop {
        // Register on every object first, so a value signaled while the task
        // runs is not missed.
        for (object, _) in objects {
            object.watch(waiter.waiter());
        }
        let mut fences = Vec::new();
        for (index, (object, point)) in objects.iter().enumerate() {
            match outcome_for(object, is_timeline, *point) {
                SyncobjOutcome::Fence(fence) if fence.is_signaled() => {
                    return Ok(Some(index as u32));
                }
                SyncobjOutcome::Fence(fence) if fence.is_chain() => {
                    fences.extend(
                        crate::sync::fence_chain::waiting_set(&fence)
                            .fences()
                            .iter()
                            .cloned(),
                    );
                }
                SyncobjOutcome::Fence(fence) => fences.push(fence),
                SyncobjOutcome::Unset => {}
            }
        }
        if fences.is_empty() {
            // Nothing is outstanding, but the object may be signaled while the
            // task sleeps (another thread's `SYNCOBJ_SIGNAL` wakes the
            // watchers), so a bounded wait still parks until the deadline or
            // the watch fires. Without a deadline it could never end.
            return match timeout {
                WaitTimeout::Infinite => Err(DrmError::InvalidInput),
                WaitTimeout::Until(_) if timeout.is_elapsed(khal::time::monotonic_time()) => {
                    Ok(None)
                }
                WaitTimeout::Until(deadline) => {
                    // The future is never ready: only the deadline (or a
                    // replacement of a watched object, which wakes the task)
                    // ends the park, and the loop then re-reads the objects.
                    let _ = crate::sync::wait::block_on_with_deadline(
                        core::future::poll_fn(|_cx| core::task::Poll::<DrmResult<()>>::Pending),
                        Some(deadline),
                    );
                    Ok(None)
                }
            };
        }
        let list = FenceList::new(fences);
        match crate::sync::wait::block_on_with_deadline(
            core::future::poll_fn(|cx| match waiter.outcome(&list, cx.waker()) {
                Some(0) => core::task::Poll::Ready(Ok(())),
                Some(error) => core::task::Poll::Ready(Err(DrmError::from_fence(error))),
                None => core::task::Poll::Pending,
            }),
            timeout.deadline(),
        ) {
            // A signal or the deadline ends the poll: report whether any value
            // is available now.
            Err(_) => return Ok(None),
            Ok(Ok(())) => {}
            Ok(Err(DrmError::TimedOut)) => return Ok(None),
            // A watched fence failed.
            Ok(Err(error)) => return Err(error),
        }
    }
}

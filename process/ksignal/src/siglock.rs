// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Signal-state ownership: one lock, one transaction, one projection.
//!
//! Everything that decides *whether a signal can be delivered to a thread*
//! lives behind [`SigLock`]: the process-directed queue, and each thread's
//! private queue, blocked set and saved sigmask. Deliverability is
//! `pending ∩ ¬blocked`, so those fields must be observed under a single
//! critical section — splitting them across locks makes "the queue is empty"
//! and "nothing is blocked" answers that can both be true at different
//! instants while the conjunction is false.
//!
//! The user-return fast path does not read this state at all. It reads a
//! *projection* ([`SigLock::may_have_shared_signal`] and each thread's `hint`)
//! which is recomputed from state by [`SigLock::republish`] — inside the
//! critical section of every mutating transaction. A projection may claim a
//! signal that is already gone (a false positive, costing one slow-path pass);
//! it can never claim there is nothing to deliver while a deliverable signal
//! is queued. That asymmetry is what makes the fast path sound, and it is why
//! no caller is allowed to write a projection: `republish` is the only writer.
//!
//! Two capabilities are kept apart so the cheap path stays cheap: [`SigView`]
//! reads state without republishing, [`SigTxn`] mutates it and records which
//! slots it touched, and only those slots are recomputed.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};

use kspin::SpinNoIrq;

use crate::{PendingSignals, SignalInfo, SignalSet, Signo};

/// Slot-table capacity reserved when the process signal state is created.
///
/// Reserving at construction keeps the first registrations growth-free, and
/// construction happens before the manager is shared, so no other thread is
/// spinning on the lock yet. A process that exceeds it pays an amortized
/// doubling of 8-byte entries, never of the 1.3 KiB slot payload.
const SLOT_TABLE_INITIAL_CAPACITY: usize = 16;

/// Number of inline bits in [`DirtySlots`]. Slots beyond this fall back to a
/// full recompute; 256 covers any realistic thread count without allocating.
const DIRTY_WORDS: usize = 4;
const DIRTY_BITS: usize = DIRTY_WORDS * 64;

/// Handle for one thread's slot in the process signal state.
///
/// The generation makes a handle unusable once its slot has been retired and
/// reused: a retained [`crate::api::ThreadSignalManager`] can never address a
/// successor thread's state, which the previous `Weak`-identity scheme
/// enforced by pointer comparison.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SlotId {
    index: usize,
    generation: u32,
}

/// Per-thread signal state owned by [`SigLock`].
///
/// Boxed inside the slot table: the inline payload is dominated by
/// `PendingSignals` (32 standard-signal cells plus 33 real-time queues), so
/// keeping it out of line keeps a retired-then-shrunk table small.
pub(crate) struct ThreadSlot {
    generation: u32,
    tid: u32,
    /// Whether this thread is still a candidate target for process-directed
    /// signals. Cleared at thread exit; the slot's own pending/blocked state
    /// stays observable by its manager until the slot is released.
    is_target: bool,
    pending: PendingSignals,
    blocked: SignalSet,
    saved_sigmask: Option<SignalSet>,
    /// Projection of `pending ∩ ¬blocked`. Only `SigLock::republish` writes it.
    hint: Arc<AtomicBool>,
}

impl ThreadSlot {
    /// Builds a slot before the critical section is taken.
    ///
    /// The payload is roughly 1.3 KiB and the projection cell is a separate
    /// allocation, so constructing it under the process-wide lock would put
    /// slab refill in a critical section that other threads' sends and
    /// slow-path dequeues spin on.
    fn allocate(tid: u32, hint: Arc<AtomicBool>) -> Box<Self> {
        Box::new(Self {
            generation: 0,
            tid,
            is_target: true,
            pending: PendingSignals::default(),
            blocked: SignalSet::default(),
            saved_sigmask: None,
            hint,
        })
    }

    /// Whether this thread has a deliverable signal, counting the
    /// process-directed queue under its own blocked mask.
    ///
    /// This is the exact per-thread predicate Linux keeps in
    /// `TIF_SIGPENDING` (its `recalc_sigpending_tsk`), so a signal every
    /// thread blocks keeps the fast path quiet instead of forcing a
    /// slow-path pass on every user return.
    fn has_deliverable(&self, shared: &PendingSignals) -> bool {
        self.pending.has_deliverable(self.blocked) || shared.has_deliverable(self.blocked)
    }
}

/// Slots touched by one transaction.
///
/// Allocation-free because transactions may run in interrupt context. Slots
/// past the inline range set `all`, which degrades to recomputing every slot
/// instead of allocating; that only happens for thread counts above
/// [`DIRTY_BITS`].
#[derive(Default)]
struct DirtySlots {
    words: [u64; DIRTY_WORDS],
    all: bool,
}

impl DirtySlots {
    fn mark_slot(&mut self, index: usize) {
        if index < DIRTY_BITS {
            self.words[index / 64] |= 1 << (index % 64);
        } else {
            self.all = true;
        }
    }

    /// Marks every slot: a process-directed queue change is visible to every
    /// thread, but each thread's answer depends on its own blocked mask.
    fn mark_all(&mut self) {
        self.all = true;
    }

    fn is_empty(&self) -> bool {
        !self.all && self.words.iter().all(|word| *word == 0)
    }

    /// Yields the touched indices, walking set bits rather than testing every
    /// index, so a transaction that touched one slot costs one bit scan.
    fn iter(&self, len: usize) -> DirtyIter<'_> {
        if self.all {
            return DirtyIter::All(0..len);
        }
        DirtyIter::Bits {
            words: &self.words,
            len,
            word: 0,
            bits: 0,
        }
    }
}

enum DirtyIter<'a> {
    All(core::ops::Range<usize>),
    Bits {
        words: &'a [u64; DIRTY_WORDS],
        len: usize,
        word: usize,
        bits: u64,
    },
}

impl Iterator for DirtyIter<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        match self {
            Self::All(range) => range.next(),
            Self::Bits {
                words,
                len,
                word,
                bits,
            } => loop {
                if *bits == 0 {
                    if *word >= DIRTY_WORDS {
                        return None;
                    }
                    *bits = words[*word];
                    *word += 1;
                    continue;
                }
                let offset = bits.trailing_zeros() as usize;
                *bits &= *bits - 1;
                let index = (*word - 1) * 64 + offset;
                // Bits past the current table length are stale (their slot was
                // popped after a thread-count peak); recomputing such a slot is
                // never needed, and a later registration re-marks its index.
                if index < *len {
                    return Some(index);
                }
            },
        }
    }
}

/// State guarded by [`SigLock`].
pub(crate) struct SigInner {
    /// Process-directed queue (Linux `signal->shared_pending`).
    shared: PendingSignals,
    /// Per-thread state, indexed by [`SlotId::index`].
    slots: Vec<Option<Box<ThreadSlot>>>,
    /// Generation source for slot handles.
    next_generation: u32,
}

impl SigInner {
    fn live_index(&self, id: SlotId) -> Option<usize> {
        let slot = self.slots.get(id.index)?.as_deref()?;
        (slot.generation == id.generation).then_some(id.index)
    }

    fn live(&self, id: SlotId) -> Option<&ThreadSlot> {
        self.slots
            .get(id.index)?
            .as_deref()
            .filter(|slot| slot.generation == id.generation)
    }

    fn live_mut(&mut self, id: SlotId) -> Option<&mut ThreadSlot> {
        let index = self.live_index(id)?;
        self.slots[index].as_deref_mut()
    }

    /// Selects a delivery target for a process-directed signal.
    ///
    /// Returns the first live thread that does not block `signo`, and whether
    /// any live thread blocks it.
    fn find_target(&self, signo: Signo) -> (Option<u32>, bool) {
        let mut target = None;
        let mut has_blocked = false;
        for slot in self.slots.iter().flatten().filter(|slot| slot.is_target) {
            if slot.blocked.has(signo) {
                has_blocked = true;
            } else if target.is_none() {
                target = Some(slot.tid);
            }
        }
        (target, has_blocked)
    }
}

/// Drops the signals the kernel never blocks.
///
/// Linux strips `SIGKILL`/`SIGSTOP` in `set_current_blocked()` and
/// `do_sigaction()` because "it is wrong to change `->blocked` directly, this
/// helper should be used". Here the helper is the only write entry, so every
/// mask writer — including the `rt_sigreturn` path, whose mask comes from the
/// user-writable frame — inherits the invariant instead of re-deriving it.
fn strip_unblockable(mut set: SignalSet) -> SignalSet {
    set.remove(Signo::SIGKILL);
    set.remove(Signo::SIGSTOP);
    set
}

/// One dequeued signal together with the blocked mask it was dequeued under.
///
/// `current_blocked` is captured in the same critical section as the dequeue
/// so the caller never restores a mask snapshot taken under a different lock.
pub(crate) struct Dequeued {
    pub(crate) sig: SignalInfo,
    pub(crate) current_blocked: SignalSet,
}

/// The process-wide critical section for signal deliverability.
pub(crate) struct SigLock {
    inner: SpinNoIrq<SigInner>,
}

impl SigLock {
    pub(crate) fn new() -> Self {
        Self {
            inner: SpinNoIrq::new(SigInner {
                shared: PendingSignals::default(),
                slots: Vec::with_capacity(SLOT_TABLE_INITIAL_CAPACITY),
                next_generation: 0,
            }),
        }
    }

    /// Registers a thread and returns its handle plus its projection cell.
    ///
    /// Both allocations happen before the lock is taken; the transaction only
    /// links the prepared slot into the table and marks it for publication.
    /// The table is pre-sized at construction, so linking is allocation-free
    /// until a process exceeds that capacity; past it the table may double,
    /// which moves 8-byte entries rather than the slot payload.
    pub(crate) fn register(&self, tid: u32) -> (SlotId, Arc<AtomicBool>) {
        let hint = Arc::new(AtomicBool::new(false));
        let slot = ThreadSlot::allocate(tid, hint.clone());
        let slot_id = self.transact(|txn| txn.register(slot));
        (slot_id, hint)
    }

    /// Runs `f` with read-only access to the guarded state.
    ///
    /// Reads cannot invalidate a projection, so this does not republish; that
    /// keeps query-only paths (`blocked`, `pending`, `signalfd` masks) free of
    /// the per-transaction recompute cost.
    pub(crate) fn inspect<R>(&self, f: impl FnOnce(&SigView<'_>) -> R) -> R {
        let inner = self.inner.lock();
        f(&SigView { inner: &inner })
    }

    /// Runs `f` as a single mutating transaction.
    ///
    /// State changes happen inside one critical section, the projections of
    /// the slots `f` actually touched are recomputed from the resulting state
    /// before the lock is released, and nothing is published from outside it.
    pub(crate) fn transact<R>(&self, f: impl FnOnce(&mut SigTxn<'_>) -> R) -> R {
        let mut inner = self.inner.lock();
        let mut txn = SigTxn {
            inner: &mut inner,
            dirty: DirtySlots::default(),
        };
        let result = f(&mut txn);
        if !txn.dirty.is_empty() {
            self.republish(txn.inner, &txn.dirty);
        }
        result
    }

    /// Recomputes the projections of the slots a transaction touched.
    ///
    /// This is the only place a projection is written, and it runs inside the
    /// critical section of the transaction that changed the state, so a
    /// concurrent sender cannot have its notification overwritten by a stale
    /// observation.
    fn republish(&self, inner: &SigInner, dirty: &DirtySlots) {
        for index in dirty.iter(inner.slots.len()) {
            let Some(slot) = inner.slots.get(index).and_then(Option::as_deref) else {
                continue;
            };
            let deliverable = slot.has_deliverable(&inner.shared);
            if slot.hint.load(Ordering::Relaxed) != deliverable {
                slot.hint.store(deliverable, Ordering::Release);
            }
            debug_assert_eq!(slot.hint.load(Ordering::Acquire), deliverable);
        }
    }
}

/// Read-only capability on the guarded state.
///
/// Held for the duration of [`SigLock::inspect`]; it cannot outlive the
/// critical section and cannot mutate anything.
pub(crate) struct SigView<'a> {
    inner: &'a SigInner,
}

impl SigView<'_> {
    /// Selects a delivery target for a process-directed signal.
    ///
    /// Returns the first live thread that does not block `signo`, and whether
    /// any live thread blocks it.
    pub(crate) fn find_target(&self, signo: Signo) -> (Option<u32>, bool) {
        self.inner.find_target(signo)
    }

    /// Returns the thread's blocked set.
    pub(crate) fn blocked(&self, id: SlotId) -> SignalSet {
        self.inner
            .live(id)
            .map_or_else(SignalSet::default, |slot| slot.blocked)
    }

    /// Returns whether the thread currently blocks `signo`.
    pub(crate) fn blocked_has(&self, id: SlotId, signo: Signo) -> bool {
        self.blocked(id).has(signo)
    }

    /// Returns the thread's private pending set.
    pub(crate) fn pending(&self, id: SlotId) -> SignalSet {
        self.inner
            .live(id)
            .map_or_else(SignalSet::default, |slot| slot.pending.set)
    }

    /// Returns the process-directed pending set.
    pub(crate) fn shared_pending(&self) -> SignalSet {
        self.inner.shared.set
    }
}

/// The only handle that can mutate signal state.
///
/// It is constructed exclusively by [`SigLock::transact`], so no code outside
/// a transaction can reach the guarded fields; and because it borrows the
/// guarded state mutably, it cannot outlive the critical section. Every
/// mutating verb records the slot or queue it touched, so the closing
/// recompute stays proportional to what actually changed.
pub(crate) struct SigTxn<'a> {
    inner: &'a mut SigInner,
    dirty: DirtySlots,
}

impl<'a> SigTxn<'a> {
    /// Read-only access to the same critical section.
    pub(crate) fn view(&self) -> SigView<'_> {
        SigView { inner: self.inner }
    }

    /// Links a slot prepared by [`SigLock::register`] and publishes it.
    pub(crate) fn register(&mut self, mut slot: Box<ThreadSlot>) -> SlotId {
        let generation = self.inner.next_generation;
        self.inner.next_generation = generation.wrapping_add(1);
        slot.generation = generation;
        let index = match self.inner.slots.iter().position(Option::is_none) {
            Some(index) => {
                self.inner.slots[index] = Some(slot);
                index
            }
            None => {
                self.inner.slots.push(Some(slot));
                self.inner.slots.len() - 1
            }
        };
        self.dirty.mark_slot(index);
        SlotId { index, generation }
    }

    /// Releases a thread's slot once its manager is dropped, and lets the
    /// table shrink again after a thread-count peak.
    ///
    /// Idempotent and generation-compared, so releasing after reuse cannot
    /// free a successor.
    pub(crate) fn retire(&mut self, id: SlotId) {
        let Some(index) = self.inner.live_index(id) else {
            return;
        };
        self.inner.slots[index] = None;
        while self.inner.slots.last().is_some_and(Option::is_none) {
            self.inner.slots.pop();
        }
    }

    /// Removes a thread from process-directed selection at exit.
    ///
    /// Its private pending queue, blocked mask and saved sigmask are kept: the
    /// thread still owns them while it unwinds, and a fatal pending signal
    /// must remain observable until the task is released.
    pub(crate) fn leave_group(&mut self, id: SlotId) {
        if let Some(slot) = self.inner.live_mut(id) {
            slot.is_target = false;
        }
    }

    /// Rebinds a slot to a new thread number after an `exec` identity takeover.
    pub(crate) fn set_tid(&mut self, id: SlotId, tid: u32) {
        if let Some(slot) = self.inner.live_mut(id) {
            slot.tid = tid;
        }
    }

    /// Queues a thread-directed signal. Returns whether it became pending.
    ///
    /// This is the one place a send allocates, and only when the queue accepts
    /// the signal (a coalesced standard signal allocates nothing). The
    /// allocation therefore lands inside the critical section, matching Linux,
    /// where `__send_signal_locked` calls `__sigqueue_alloc` with `GFP_ATOMIC`
    /// under `sighand->siglock` and skips it for a coalesced signal or
    /// `SIGKILL`.
    pub(crate) fn put_thread(&mut self, id: SlotId, sig: SignalInfo) -> bool {
        let Some(index) = self.inner.live_index(id) else {
            return false;
        };
        let added = self.inner.slots[index]
            .as_deref_mut()
            .is_some_and(|slot| slot.pending.put_signal(sig));
        if added {
            self.dirty.mark_slot(index);
        }
        added
    }

    /// Queues a process-directed signal. Returns whether it became pending.
    ///
    /// Allocates on acceptance for the same reason as [`Self::put_thread`].
    pub(crate) fn put_shared(&mut self, sig: SignalInfo) -> bool {
        let added = self.inner.shared.put_signal(sig);
        if added {
            self.dirty.mark_all();
        }
        added
    }

    /// Dequeues the next signal deliverable to `id` under its blocked mask,
    /// preferring the thread-private queue over the process-directed one.
    pub(crate) fn take(&mut self, id: SlotId) -> Option<Dequeued> {
        self.take_inner(id, None)
    }

    /// Dequeues the next signal matching an explicit mask.
    ///
    /// Used by `signalfd` and `sigtimedwait`, whose mask is supplied by the
    /// caller rather than derived from this thread's blocked set.
    pub(crate) fn take_masked(&mut self, id: SlotId, mask: &SignalSet) -> Option<Dequeued> {
        self.take_inner(id, Some(*mask))
    }

    fn take_inner(&mut self, id: SlotId, explicit: Option<SignalSet>) -> Option<Dequeued> {
        let index = self.inner.live_index(id)?;
        let current_blocked = self.inner.slots[index].as_deref()?.blocked;
        let mask = explicit.unwrap_or(!current_blocked);
        if let Some(sig) = self.inner.slots[index]
            .as_deref_mut()?
            .pending
            .dequeue_signal(&mask)
        {
            self.dirty.mark_slot(index);
            return Some(Dequeued {
                sig,
                current_blocked,
            });
        }
        let sig = self.inner.shared.dequeue_signal(&mask)?;
        // The shared queue changed, so every thread's answer may have.
        self.dirty.mark_all();
        Some(Dequeued {
            sig,
            current_blocked,
        })
    }

    /// Replaces the thread's blocked set, returning the previous one.
    ///
    /// `SIGKILL` and `SIGSTOP` are always dropped from `set`.
    pub(crate) fn set_blocked(&mut self, id: SlotId, set: SignalSet) -> SignalSet {
        let set = strip_unblockable(set);
        let Some(index) = self.inner.live_index(id) else {
            return SignalSet::default();
        };
        self.dirty.mark_slot(index);
        match self.inner.slots[index].as_deref_mut() {
            Some(slot) => core::mem::replace(&mut slot.blocked, set),
            None => SignalSet::default(),
        }
    }

    /// Adds signals to the thread's blocked set.
    ///
    /// `SIGKILL` and `SIGSTOP` are always dropped from `extra`, so a handler
    /// mask cannot make an unblockable signal blockable either.
    pub(crate) fn add_blocked(&mut self, id: SlotId, extra: SignalSet) {
        let extra = strip_unblockable(extra);
        let Some(index) = self.inner.live_index(id) else {
            return;
        };
        self.dirty.mark_slot(index);
        if let Some(slot) = self.inner.slots[index].as_deref_mut() {
            slot.blocked |= extra;
        }
    }

    /// Saves a blocked-mask snapshot for the next caught-signal frame.
    pub(crate) fn set_saved_sigmask(&mut self, id: SlotId, mask: SignalSet) {
        if let Some(slot) = self.inner.live_mut(id) {
            slot.saved_sigmask = Some(mask);
        }
    }

    /// Takes and clears the saved blocked-mask snapshot.
    ///
    /// The snapshot is not part of the deliverability predicate, so no
    /// projection depends on it.
    pub(crate) fn take_saved_sigmask(&mut self, id: SlotId) -> Option<SignalSet> {
        self.inner
            .live_mut(id)
            .and_then(|slot| slot.saved_sigmask.take())
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Allocation-free wait slots and the latch future for the blocking mutex.

use core::{
    cell::UnsafeCell,
    marker::PhantomPinned,
    pin::Pin,
    sync::atomic::Ordering,
    task::{Context, Poll, Waker},
};

use intrusive_collections::{LinkedList, LinkedListLink, UnsafeRef, intrusive_adapter};

mod latch {
    //! The waiter's monotonic waker handshake, encoded in one atomic word.

    use core::sync::atomic::{AtomicU8, Ordering};

    /// Waker handshake states, monotonic:
    /// `Empty -> Registered -> Claimed -> Done`.
    ///
    /// `Empty` has no waker; `Registered` stored one and awaits the unlocker;
    /// `Claimed` means the unlocker is taking the waker (poll must not touch
    /// the slot); `Done` means the unlocker finished all node access, so a poll
    /// that observes it may return ready and drop the node.
    #[repr(u8)]
    #[derive(Clone, Copy, Debug)]
    pub(super) enum LatchState {
        Empty      = 0,
        Registered = 1,
        Claimed    = 2,
        Done       = 3,
    }

    impl LatchState {
        /// Decodes a raw state word.
        ///
        /// - `raw`: the atomic state word; only the four discriminants are
        ///   ever stored.
        ///
        /// Returns: the matching latch state.
        ///
        /// # Panics
        ///
        /// Panics on a value outside the encoded set, which no writer stores.
        fn from_raw(raw: u8) -> Self {
            match raw {
                0 => Self::Empty,
                1 => Self::Registered,
                2 => Self::Claimed,
                3 => Self::Done,
                _ => unreachable!("invalid waiter latch state: {raw:#x}"),
            }
        }
    }

    /// The atomic latch state word, which always holds a valid [`LatchState`].
    #[repr(transparent)]
    pub(super) struct AtomicLatchState(AtomicU8);

    impl AtomicLatchState {
        /// Creates a latch state word holding `state`.
        ///
        /// - `state`: the initial latch state.
        pub(super) const fn new(state: LatchState) -> Self {
            Self(AtomicU8::new(state as u8))
        }

        /// Loads the current latch state.
        ///
        /// - `ordering`: the load ordering.
        ///
        /// Returns: the decoded latch state.
        pub(super) fn load(&self, ordering: Ordering) -> LatchState {
            LatchState::from_raw(self.0.load(ordering))
        }

        /// Stores `state`.
        ///
        /// - `state`: the latch state to publish.
        /// - `ordering`: the store ordering.
        pub(super) fn store(&self, state: LatchState, ordering: Ordering) {
            self.0.store(state as u8, ordering);
        }

        /// Compares and exchanges the latch state.
        ///
        /// - `current`: the state expected in the word.
        /// - `new`: the state written on success.
        /// - `success`: the ordering on success.
        /// - `failure`: the ordering on failure.
        ///
        /// Returns: `Ok(previous)` on success, `Err(actual)` with the observed
        /// state otherwise.
        pub(super) fn compare_exchange(
            &self,
            current: LatchState,
            new: LatchState,
            success: Ordering,
            failure: Ordering,
        ) -> Result<LatchState, LatchState> {
            match self
                .0
                .compare_exchange(current as u8, new as u8, success, failure)
            {
                Ok(raw) => Ok(LatchState::from_raw(raw)),
                Err(raw) => Err(LatchState::from_raw(raw)),
            }
        }
    }
}

use self::latch::{AtomicLatchState, LatchState};

/// A stack-pinned node linking one blocked task into a mutex wait list.
///
/// The node is pushed inside a `wait_state` critical section and popped by the
/// unlocking task before it publishes the latch; it must never move while linked.
pub(crate) struct Waiter {
    link: LinkedListLink,
    /// Waker handshake state, advanced monotonically by `poll` and `latch`.
    ///
    /// `Empty` before the first poll stores a waker, `Registered` once it has,
    /// `Claimed` while the unlocker takes it, and `Done` once the unlocker
    /// finished all node access. `Done` is the only state in which a poll frees
    /// the node, so publishing it after the waker take is what keeps the pinned
    /// frame alive until the unlocker is done with it.
    state: AtomicLatchState,
    /// Waker captured by the node's future on first poll and taken by `latch`.
    ///
    /// Written only while `state` moves `Empty -> Registered` (by `poll`) and
    /// read only after `latch` wins `Registered -> Claimed`, so the two accesses
    /// never race; the `AcqRel` transitions on `state` order them.
    waker: UnsafeCell<Option<Waker>>,
    _pin: PhantomPinned,
}

impl Waiter {
    /// Creates an unlinked waiter slot with no registered waker.
    pub(crate) fn new() -> Self {
        Waiter {
            link: LinkedListLink::new(),
            state: AtomicLatchState::new(LatchState::Empty),
            waker: UnsafeCell::new(None),
            _pin: PhantomPinned,
        }
    }

    /// Publishes the wake and takes any waker registered before it.
    ///
    /// Returns: the waker stored by the first poll; `None` when the wake beat
    /// the first poll or the waker was already taken.
    ///
    /// Locking: lock-free. `Done` is published only after the node's last
    /// access, and a poll frees the node only on `Done`, so the pinned frame
    /// cannot be dropped under this call. The caller must not touch the node
    /// after it returns; `wake_by_ref` runs on the returned waker.
    pub(crate) fn latch(&self) -> Option<Waker> {
        loop {
            match self.state.load(Ordering::Acquire) {
                LatchState::Done => return None,
                // Wake before the first poll: leave `Done` so later polls
                // resolve without parking, and take nothing. A racing
                // registration makes the CAS fail and the loop re-reads state.
                LatchState::Empty => {
                    if self
                        .state
                        .compare_exchange(
                            LatchState::Empty,
                            LatchState::Done,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        return None;
                    }
                }
                // Claim the waker so a poll stops touching the slot, take it,
                // then publish `Done` after that last access.
                LatchState::Registered => {
                    if self
                        .state
                        .compare_exchange(
                            LatchState::Registered,
                            LatchState::Claimed,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        // SAFETY: winning `Registered -> Claimed` makes this
                        // the only reader of the slot (a poll that observes
                        // `Claimed` leaves it alone), and the AcqRel CAS orders
                        // the poll's write before this read.
                        let waker = unsafe { (*self.waker.get()).take() };
                        // Publish `Done` only after the slot access: a poll
                        // that observes `Done` may free the node, so nothing
                        // may touch it after this store.
                        self.state.store(LatchState::Done, Ordering::Release);
                        return waker;
                    }
                }
                // Only this single unlocker sets `Claimed`; observing it again
                // would be a stale read, so retry.
                LatchState::Claimed => {}
            }
        }
    }
}

intrusive_adapter!(pub(crate) WaiterAdapter = UnsafeRef<Waiter>: Waiter {
    link => LinkedListLink
});

/// The mutex's own FIFO wait list, guarded by `wait_state`.
///
/// All mutations and `waiters` transitions happen inside the guard.
pub(crate) struct WaitState {
    pub(crate) list: LinkedList<WaiterAdapter>,
}

impl WaitState {
    /// Creates an empty wait state.
    pub(crate) const fn new() -> Self {
        WaitState {
            list: LinkedList::new(WaiterAdapter::NEW),
        }
    }
}

// SAFETY: the intrusive links are only reachable while holding the enclosing
// `SpinNoPreempt`; no list operation runs outside that lock, so the `Cell`-based
// links are never touched concurrently. Each node's waker slot is a separate
// `UnsafeCell`: a poll writes it on `Empty -> Registered`, and `latch` reads it
// only after winning `Registered -> Claimed`, so the two accesses never run
// concurrently and the `AcqRel` transitions on `state` order them. `Done` is
// published after that read, so a poll frees the frame only after the unlocker
// is finished with it. Moving the list across tasks therefore cannot race
// waker access or the frame lifetime.
unsafe impl Send for WaitState {}

/// A future resolving once the waiter's latch is set.
///
/// The future is driven by the host's `block_on`; it stores the poll waker in
/// the node so the unlocking task can wake this task.
pub(crate) struct WaitFuture<'a> {
    node: &'a Waiter,
}

impl<'a> WaitFuture<'a> {
    /// Creates a future that resolves once `node`'s latch is set.
    ///
    /// - `node`: the pinned waiter node whose latch and waker slot the future
    ///   uses.
    pub(crate) fn new(node: &'a Waiter) -> Self {
        WaitFuture { node }
    }
}

impl Future for WaitFuture<'_> {
    type Output = ();

    /// Registers the poll waker, or resolves once the unlocker is done.
    ///
    /// Locking: lock-free; runs in task context only.
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let node = self.as_ref().get_ref().node;
        match node.state.load(Ordering::Acquire) {
            // The unlocker finished all node access; the frame may be freed.
            LatchState::Done => return Poll::Ready(()),
            // The unlocker is taking the waker (`Claimed`) or a waker is
            // already registered (`Registered`): stay parked and leave the slot
            // alone.
            LatchState::Claimed | LatchState::Registered => return Poll::Pending,
            LatchState::Empty => {}
        }
        // SAFETY: the slot is written only in the `Empty -> Registered`
        // transition; the CAS below publishes the write before any latch that
        // observes `Registered` may read it.
        unsafe { *node.waker.get() = Some(cx.waker().clone()) };
        // A failed CAS means a latch claimed the wake straight from `Empty` and
        // published `Done`, so the future resolves now.
        match node.state.compare_exchange(
            LatchState::Empty,
            LatchState::Registered,
            Ordering::Release,
            Ordering::Acquire,
        ) {
            Ok(_) => Poll::Pending,
            Err(_) => Poll::Ready(()),
        }
    }
}

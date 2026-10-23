// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Allocation-free synchronous completion tokens and scoped stack waiters.
//!
//! The host supplies a [`SyncWake`] and a matching parking closure. This crate
//! owns neither tasks nor scheduler state. Notifications run under the completion
//! lock; parking runs outside it. No standard task waker or poll registration is
//! constructed. See the crate-local design and security documents for invariants.

extern crate alloc;

use alloc::sync::Arc;
use core::{marker::PhantomPinned, pin::pin, ptr::NonNull};

use intrusive_collections::{LinkedList, LinkedListLink, UnsafeRef, intrusive_adapter};
use kspin::SpinNoIrq;

const COMPLETE_ALL: usize = usize::MAX;

/// A synchronous notification capability borrowed for one blocking wait.
///
/// Notifications are wake hints, not completion tokens. The matching parker
/// must remember notification before parking and tolerate repeated notification.
///
/// # Safety
/// Implementations must be IRQ-safe, nonblocking, allocation-free, and must not
/// unwind. `notify` runs with the completion's IRQ-saving spinlock held. It must
/// not reenter that completion, directly or indirectly, or acquire a lock whose
/// holder can wait for that completion lock. It must not retain references or
/// pointers borrowed from this call for later access. All accesses to shared
/// notification state must be synchronized. These obligations also apply to any
/// callbacks invoked by `notify`.
pub unsafe trait SyncWake: Sync {
    /// Records a wake hint and makes the associated waiter runnable if needed.
    fn notify(&self);
}

struct WaitNode {
    links: LinkedListLink,
    data: NonNull<()>,
    notify: unsafe fn(NonNull<()>),
    _pin: PhantomPinned,
}

intrusive_adapter!(WaitAdapter = UnsafeRef<WaitNode>: WaitNode { links => LinkedListLink });

/// # Safety
/// `data` must point to a live, shared `W` for the entire call. The enclosing
/// completion lock must exclude waiter cleanup; `W` supplies the IRQ contract.
unsafe fn notify_borrowed<W: SyncWake>(data: NonNull<()>) {
    // SAFETY: wait_with pairs this trampoline with its borrowed W; the private
    // registration retains that borrow until cleanup acquires the same lock.
    let wake = unsafe { data.cast::<W>().as_ref() };
    wake.notify();
}

struct CompletionState {
    done: usize,
    waiters: LinkedList<WaitAdapter>,
}

// SAFETY: Only Completion's lock permits access to the list and its links.
// wait_with pins and retains every node until locked cleanup finishes. The
// erased pointee is always a shared SyncWake (Sync), never moved or owned here.
unsafe impl Send for CompletionState {}

impl CompletionState {
    fn try_wait(&mut self) -> bool {
        if self.done == 0 {
            return false;
        }
        if self.done != COMPLETE_ALL {
            self.done -= 1;
        }
        true
    }

    fn wake_one(&mut self) -> bool {
        let Some(node) = self.waiters.pop_front() else {
            return false;
        };
        // SAFETY: Only wait_with constructs nodes, pairing data with the correct
        // trampoline. The lock keeps both borrows alive through notification.
        unsafe { (node.notify)(node.data) };
        true
    }

    fn wake_all(&mut self) -> usize {
        let mut count = 0;
        while self.wake_one() {
            count += 1;
        }
        count
    }
}

/// Linux-style completion tokens with private, pinned stack registrations.
///
/// Construction and wait registration do not allocate. Signaling and queries
/// are non-sleeping and IRQ-safe once the platform supports [`SpinNoIrq`].
/// Waiters consume one token each, except that `complete_all` remains sticky
/// until `reinit`. Notification order is FIFO; token acquisition is not reserved
/// for the notified waiter, so a concurrent `try_wait` may consume its token.
pub struct Completion {
    state: SpinNoIrq<CompletionState>,
}

impl Completion {
    /// Creates an empty completion without allocating or requiring a task.
    pub const fn new() -> Self {
        Self {
            state: SpinNoIrq::new(CompletionState {
                done: 0,
                waiters: LinkedList::new(WaitAdapter::NEW),
            }),
        }
    }

    /// Consumes one available token, or returns false without waiting.
    ///
    /// Sticky completion always returns true without consuming its state.
    pub fn try_wait(&self) -> bool {
        self.state.lock().try_wait()
    }

    /// Reports whether a token or sticky completion is currently available.
    ///
    /// This snapshot neither consumes a token nor proves that waiters finished.
    pub fn is_completed(&self) -> bool {
        self.state.lock().done != 0
    }

    /// Clears completion tokens without removing registered waiters.
    ///
    /// The caller must ensure all waiters relying on the preceding complete-all
    /// state have finished observing it; reset must not abandon a waiter that
    /// still requires that state. A DeferredWake may run after reset: it only
    /// prompts current waiters to recheck and never republishes completion.
    /// Predicate-based users must recheck their authoritative predicate and
    /// arrange a subsequent completion if that predicate is still false.
    pub fn reinit(&self) {
        self.state.lock().done = 0;
    }

    /// Adds one token and notifies the oldest registered waiter, if any.
    ///
    /// Returns the number notified (zero or one), not the number that consumed a
    /// token. The token count saturates at sticky completion, as in Linux.
    /// Notification runs under the completion lock and may acquire host locks;
    /// callers must respect the host's lock ordering.
    pub fn complete(&self) -> usize {
        let mut state = self.state.lock();
        state.done = state.done.saturating_add(1);
        usize::from(state.wake_one())
    }

    /// Publishes sticky completion and notifies all currently registered waiters.
    ///
    /// Returns the number notified, not the number that consumed completion.
    /// Holds the IRQ-saving lock through all callbacks: IRQ-off work is linear
    /// in waiter count. Call outside locks that conflict with host notification.
    pub fn complete_all(&self) -> usize {
        let mut state = self.state.lock();
        state.done = COMPLETE_ALL;
        state.wake_all()
    }

    /// Publishes sticky completion now and defers notification until `wake`.
    ///
    /// Use while publishing related state under an outer lock, then consume the
    /// returned handle after releasing that lock. Only the completion lock is
    /// acquired here; no host notification runs. Cloning the existing Arc does
    /// not allocate and keeps the completion alive across descriptor removal.
    /// The caller must eventually call [`DeferredWake::wake`].
    pub fn complete_all_defer_wake(self: &Arc<Self>) -> DeferredWake {
        self.state.lock().done = COMPLETE_ALL;
        DeferredWake {
            completion: self.clone(),
        }
    }

    /// Waits synchronously until one token or sticky completion is consumed.
    ///
    /// `wake` is borrowed only for this call. `park` runs outside all completion
    /// locks and may return spuriously. It must use the same notification latch
    /// as `wake`, consuming an early notification instead of sleeping past it.
    /// It must return after a notification, including one arriving between
    /// registration and parking. Callers validate sleepability before entry and
    /// must not hold any lock that forbids sleeping. No timeout or signal
    /// interruption is performed here.
    ///
    /// Registration is private and removed before return, including unwinding
    /// from `park` on platforms supporting unwinding. The completion and `wake`
    /// remain borrowed until cleanup finishes on normal return or unwind.
    ///
    /// # Safety
    /// The caller must keep the execution stack, completion, and borrowed wake
    /// storage alive until private registration cleanup finishes. This includes
    /// abnormal nonunwinding task exit: either prevent cleanup from being bypassed
    /// or retain all that storage if it is bypassed. Rust borrowing alone does
    /// not prevent a scheduler from reclaiming an abandoned stack. A host may
    /// retain a strong task/stack owner across the call; if exit skips Drop, that
    /// owner must leak rather than permit reclamation. Any wake storage outside
    /// that retained stack needs its own retention guarantee. Ordinary unwinding
    /// is supported because it executes registration cleanup.
    ///
    /// # Panics
    /// Propagates a panic from `park` after unregistering the stack waiter.
    pub unsafe fn wait_with<W: SyncWake>(&self, wake: &W, mut park: impl FnMut()) {
        with_waiter(&self.state, wake, |node| {
            loop {
                {
                    let mut state = self.state.lock();
                    if state.try_wait() {
                        return;
                    }
                    if !node.links.is_linked() {
                        // SAFETY: with_waiter pins this node and retains shared-only
                        // access until locked cleanup removes it. The node belongs
                        // only to this list, and the lock serializes link checks.
                        let entry = unsafe { UnsafeRef::from_raw(node) };
                        state.waiters.push_back(entry);
                    }
                }
                park();
            }
        });
    }
}

impl Default for Completion {
    fn default() -> Self {
        Self::new()
    }
}

/// Predicate broadcast waiting without stored completion tokens.
///
/// Notifications only prompt predicate rechecks. The predicate and its data
/// synchronization belong to the caller; notification neither consumes data nor
/// implies that the predicate is true. Uses the same scoped list as Completion.
pub struct WaitQueue {
    state: SpinNoIrq<CompletionState>,
}

impl WaitQueue {
    /// Creates an empty queue without allocating or requiring a task.
    pub const fn new() -> Self {
        Self {
            state: SpinNoIrq::new(CompletionState {
                done: 0,
                waiters: LinkedList::new(WaitAdapter::NEW),
            }),
        }
    }

    /// Removes and notifies all registered waiters, returning the count.
    ///
    /// IRQ-safe and allocation-free. Call after publishing predicate changes
    /// and releasing locks that conflict with host notification. Callbacks run
    /// under the queue lock; IRQ-off work is linear in waiter count.
    pub fn notify_all(&self) -> usize {
        self.state.lock().wake_all()
    }

    /// Registers before checking `ready`, then parks until it becomes true.
    ///
    /// Both closures run outside the queue lock. `ready` may reenter this queue
    /// (including notification), but must not sleep. It synchronizes its own
    /// predicate data. Each iteration removes any old registration and enrolls
    /// again before evaluating `ready`, covering spurious and concurrent wakes.
    /// `park` must share a persistent notification latch with `wake`, consume
    /// early hints, and tolerate spurious returns, as for Completion::wait_with.
    /// Call only in validated sleepable context, without locks forbidding sleep.
    ///
    /// # Safety
    /// The execution stack, queue, and borrowed wake storage must remain alive
    /// until private registration cleanup finishes, including when either
    /// closure bypasses Rust cleanup through nonunwinding task exit. Prevent
    /// that bypass or retain all referenced storage even if the call never
    /// returns. See [`Completion::wait_with`] for the host stack-retention
    /// contract. Ordinary unwinding runs cleanup and is supported.
    ///
    /// # Panics
    /// Propagates closure panics after unregistering when unwinding is supported.
    pub unsafe fn wait_until<W: SyncWake>(
        &self,
        wake: &W,
        mut ready: impl FnMut() -> bool,
        mut park: impl FnMut(),
    ) {
        with_waiter(&self.state, wake, |node| {
            loop {
                {
                    let mut state = self.state.lock();
                    if node.links.is_linked() {
                        // SAFETY: Only this queue can link the private pinned node;
                        // the lock preserves membership through cursor removal.
                        unsafe { state.waiters.cursor_mut_from_ptr(node) }.remove();
                    }
                    // SAFETY: The same scoped lifetime covers reenrollment and the
                    // node is unlinked under this queue's lock. No mutable node
                    // reference exists while the non-owning entry is retained.
                    let entry = unsafe { UnsafeRef::from_raw(node) };
                    state.waiters.push_back(entry);
                }
                if ready() {
                    return;
                }
                park();
            }
        });
    }
}

impl Default for WaitQueue {
    fn default() -> Self {
        Self::new()
    }
}

fn with_waiter<W: SyncWake>(
    state: &SpinNoIrq<CompletionState>,
    wake: &W,
    wait: impl FnOnce(&WaitNode),
) {
    let node = pin!(WaitNode {
        links: LinkedListLink::new(),
        data: NonNull::from(wake).cast(),
        notify: notify_borrowed::<W>,
        _pin: PhantomPinned,
    });
    let registration = Registration {
        state,
        node: node.as_ref().get_ref(),
        _wake: wake,
    };
    wait(registration.node);
}

struct Registration<'a> {
    state: &'a SpinNoIrq<CompletionState>,
    node: &'a WaitNode,
    _wake: &'a dyn SyncWake,
}

impl Drop for Registration<'_> {
    fn drop(&mut self) {
        let mut state = self.state.lock();
        if self.node.links.is_linked() {
            // SAFETY: Only this source can link the private node, and the lock
            // keeps it linked through removal. The guard cannot escape; taking
            // this lock also waits for callbacks on already-popped nodes.
            unsafe { state.waiters.cursor_mut_from_ptr(self.node) }.remove();
        }
    }
}

/// An owned notification obligation retaining the completion until dispatch.
///
/// Dropping this handle does not notify: it releases the Arc and leaves sticky
/// completion published, which can strand already parked waiters. Call `wake`
/// outside the outer lock used when publishing state. This handle contains no
/// detached waiter pointers; dispatch accesses the completion's current list.
#[must_use = "call wake after releasing outer locks to notify parked waiters"]
pub struct DeferredWake {
    completion: Arc<Completion>,
}

impl DeferredWake {
    /// Notifies all current waiters and releases this handle's completion owner.
    ///
    /// Returns the number notified. Does not republish completion state or add
    /// tokens, even if reinit ran after this handle was created. Such delayed
    /// dispatch is only a wake hint for the current list, not a generation fence.
    /// IRQ-safe, non-sleeping and allocation-free; the final Arc release
    /// may deallocate storage. The host allocator must support that context.
    pub fn wake(self) -> usize {
        self.completion.state.lock().wake_all()
    }
}

unittest::path_mod_test!(tests, "../ut-tests/kcompletion_unittest.rs");

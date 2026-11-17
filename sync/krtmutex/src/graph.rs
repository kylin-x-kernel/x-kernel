// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The inheritance graph: lock waiter trees, per-task donor trees and the
//! chain walk that keeps boosts equal to the highest donation.
//!
//! One global lock guards every tree. It is `SpinNoPreempt`: every accessor
//! runs in task context, so there is no IRQ contender. Host callbacks that
//! may take run-queue locks, and waker calls, run only after that lock is
//! released.

use alloc::sync::Arc;
use core::{
    cell::UnsafeCell,
    cmp::Reverse,
    sync::atomic::{AtomicU64, Ordering},
    task::Waker,
};

use intrusive_collections::{KeyAdapter, RBTree, RBTreeAtomicLink, intrusive_adapter};
use kerrno::{KError, KErrorKind, KResult};
use kspin::SpinNoPreempt;

use crate::PiHost;

/// Owner chain longer than this is reported as deadlock. Same bound as Linux
/// `max_lock_depth`.
const MAX_PI_DEPTH: usize = 1024;

static NEXT_SEQ: AtomicU64 = AtomicU64::new(1);

kspin::static_lock! {
    static PI_GRAPH: SpinNoPreempt<()> = SpinNoPreempt::new(());
}

/// Proof that [`PI_GRAPH`] is held. Graph fields are reachable only through it.
pub(crate) struct GraphToken(());

pub(crate) fn with_graph<R>(body: impl FnOnce(&GraphToken) -> R) -> R {
    let _guard = PI_GRAPH.lock();
    body(&GraphToken(()))
}

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
struct WaitKey {
    /// Higher priority sorts first. `RBTree::front` is the minimum key.
    rank: Reverse<u8>,
    seq: u64,
}

impl WaitKey {
    fn new(priority: u8) -> Self {
        Self {
            rank: Reverse(priority),
            seq: NEXT_SEQ.fetch_add(1, Ordering::Relaxed),
        }
    }

    fn priority(self) -> u8 {
        self.rank.0
    }
}

/// One task waiting for one lock.
pub(crate) struct Waiter<H: PiHost> {
    pub(crate) task: H::Task,
    pub(crate) lock: Arc<LockState<H>>,
    lock_link: RBTreeAtomicLink,
    donor_link: RBTreeAtomicLink,
    key: UnsafeCell<WaitKey>,
    is_granted: UnsafeCell<bool>,
    waker: UnsafeCell<Option<Waker>>,
}

// SAFETY: `key`, `is_granted` and `waker` are read and written only while
// `PI_GRAPH` is held, and `key` changes only while both links are unlinked.
// The links are synchronized by that same lock. `task` and `lock` are
// immutable after construction and are `Send + Sync` themselves.
unsafe impl<H: PiHost> Send for Waiter<H> {}
// SAFETY: see the `Send` justification above.
unsafe impl<H: PiHost> Sync for Waiter<H> {}

impl<H: PiHost> Waiter<H> {
    pub(crate) fn new(task: H::Task, lock: Arc<LockState<H>>) -> Self {
        let priority = H::donor_priority(&task);
        Self {
            task,
            lock,
            lock_link: RBTreeAtomicLink::new(),
            donor_link: RBTreeAtomicLink::new(),
            key: UnsafeCell::new(WaitKey::new(priority)),
            is_granted: UnsafeCell::new(false),
            waker: UnsafeCell::new(None),
        }
    }

    fn key(&self, _: &GraphToken) -> WaitKey {
        // SAFETY: `PI_GRAPH` is held. The key is not written while a link is live.
        unsafe { *self.key.get() }
    }

    fn set_priority(&self, priority: u8, _: &GraphToken) {
        // SAFETY: `PI_GRAPH` is held and the caller unlinked both trees first.
        unsafe {
            (*self.key.get()).rank = Reverse(priority);
        }
    }

    pub(crate) fn is_granted(&self, _: &GraphToken) -> bool {
        // SAFETY: `PI_GRAPH` is held.
        unsafe { *self.is_granted.get() }
    }

    fn grant(&self, _: &GraphToken) -> Option<Waker> {
        // SAFETY: `PI_GRAPH` is held.
        unsafe {
            *self.is_granted.get() = true;
        }
        // SAFETY: `PI_GRAPH` is still held, excluding waker registration.
        unsafe { (*self.waker.get()).take() }
    }

    pub(crate) fn is_queued(&self) -> bool {
        self.lock_link.is_linked()
    }

    pub(crate) fn register_waker(&self, waker: &Waker, _: &GraphToken) {
        // SAFETY: `PI_GRAPH` is held.
        let slot = unsafe { &mut *self.waker.get() };
        if !slot.as_ref().is_some_and(|old| old.will_wake(waker)) {
            *slot = Some(waker.clone());
        }
    }
}

intrusive_adapter!(LockAdapter<H> = Arc<Waiter<H>>: Waiter<H> { lock_link => RBTreeAtomicLink } where H: PiHost);
intrusive_adapter!(DonorAdapter<H> = Arc<Waiter<H>>: Waiter<H> { donor_link => RBTreeAtomicLink } where H: PiHost);

impl<'a, H: PiHost> KeyAdapter<'a> for LockAdapter<H> {
    type Key = WaitKey;

    fn get_key(&self, value: &'a Waiter<H>) -> WaitKey {
        // SAFETY: the tree calls this only from operations under `PI_GRAPH`,
        // and the key does not change while `lock_link` is linked.
        unsafe { *value.key.get() }
    }
}

impl<'a, H: PiHost> KeyAdapter<'a> for DonorAdapter<H> {
    type Key = WaitKey;

    fn get_key(&self, value: &'a Waiter<H>) -> WaitKey {
        // SAFETY: same critical section as [`LockAdapter`]. `donor_link` is
        // unlinked before the key changes.
        unsafe { *value.key.get() }
    }
}

pub(crate) struct LockInner<H: PiHost> {
    pub(crate) owner: Option<H::Task>,
    /// The waiter linked into the owner's donor tree.
    top: Option<Arc<Waiter<H>>>,
    waiters: RBTree<LockAdapter<H>>,
}

/// Owner and waiters of one lock. Waiters keep it alive through `Arc`.
pub(crate) struct LockState<H: PiHost> {
    inner: UnsafeCell<LockInner<H>>,
}

// SAFETY: every access to `inner` happens while `PI_GRAPH` is held, and the
// task references and waiters it stores are `Send + Sync`.
unsafe impl<H: PiHost> Send for LockState<H> {}
// SAFETY: see the `Send` justification above.
unsafe impl<H: PiHost> Sync for LockState<H> {}

impl<H: PiHost> LockState<H> {
    pub(crate) fn new() -> Self {
        Self {
            inner: UnsafeCell::new(LockInner {
                owner: None,
                top: None,
                waiters: RBTree::new(LockAdapter::NEW),
            }),
        }
    }

    #[expect(
        clippy::mut_from_ref,
        reason = "CG-024-lint-exceptions: clippy::mut_from_ref: PI_GRAPH owns this cell; the \
                  token proves exclusion and graph callers do not retain overlapping borrows"
    )]
    pub(crate) fn inner(&self, _: &GraphToken) -> &mut LockInner<H> {
        // SAFETY: `GraphToken` exists only while `PI_GRAPH` is held, which is
        // not re-entered, so no other reference to the cell is live.
        unsafe { &mut *self.inner.get() }
    }

    pub(crate) fn is_owned_by(&self, task: &H::Task, token: &GraphToken) -> bool {
        self.inner(token)
            .owner
            .as_ref()
            .is_some_and(|owner| H::same_task(owner, task))
    }
}

/// Inheritance state embedded in each task by the host.
pub struct PiTaskNode<H: PiHost> {
    blocked_on: UnsafeCell<Option<Arc<Waiter<H>>>>,
    donors: UnsafeCell<RBTree<DonorAdapter<H>>>,
}

// SAFETY: both cells are accessed only while `PI_GRAPH` is held.
unsafe impl<H: PiHost> Send for PiTaskNode<H> {}
// SAFETY: see the `Send` justification above.
unsafe impl<H: PiHost> Sync for PiTaskNode<H> {}

impl<H: PiHost> Default for PiTaskNode<H> {
    fn default() -> Self {
        Self::new()
    }
}

impl<H: PiHost> PiTaskNode<H> {
    /// A task that waits on nothing and holds no contended lock.
    pub const fn new() -> Self {
        Self {
            blocked_on: UnsafeCell::new(None),
            donors: UnsafeCell::new(RBTree::new(DonorAdapter::NEW)),
        }
    }

    pub(crate) fn blocked(&self, _: &GraphToken) -> Option<Arc<Waiter<H>>> {
        // SAFETY: `PI_GRAPH` is held.
        unsafe { (*self.blocked_on.get()).clone() }
    }

    fn set_blocked(&self, waiter: Option<Arc<Waiter<H>>>, _: &GraphToken) {
        // SAFETY: `PI_GRAPH` is held.
        unsafe {
            *self.blocked_on.get() = waiter;
        }
    }

    #[expect(
        clippy::mut_from_ref,
        reason = "CG-024-lint-exceptions: clippy::mut_from_ref: the donor tree is owned by \
                  PI_GRAPH, which the token proves held; graph callers do not overlap borrows"
    )]
    fn donors(&self, _: &GraphToken) -> &mut RBTree<DonorAdapter<H>> {
        // SAFETY: `PI_GRAPH` is held and not re-entered.
        unsafe { &mut *self.donors.get() }
    }
}

pub(crate) fn next_owner<H: PiHost>(lock: &LockState<H>, token: &GraphToken) -> Option<H::Task> {
    lock.inner(token)
        .waiters
        .front()
        .get()
        .map(|waiter| waiter.task.clone())
}

fn donor_boost<H: PiHost>(task: &H::Task, token: &GraphToken) -> u8 {
    H::node(task)
        .donors(token)
        .front()
        .get()
        .map_or(0, |top| top.key(token).priority())
}

/// Rejects a wait that would close a cycle back to `waiter`.
pub(crate) fn ensure_no_deadlock<H: PiHost>(
    owner: &H::Task,
    waiter: &H::Task,
    token: &GraphToken,
) -> KResult<()> {
    let mut current = owner.clone();
    for _ in 0..MAX_PI_DEPTH {
        if H::same_task(&current, waiter) {
            return Err(KError::from(KErrorKind::Deadlock));
        }
        let Some(next) = blocked_owner::<H>(&current, token) else {
            return Ok(());
        };
        current = next;
    }
    Err(KError::from(KErrorKind::Deadlock))
}

fn blocked_owner<H: PiHost>(task: &H::Task, token: &GraphToken) -> Option<H::Task> {
    let waiter = H::node(task).blocked(token)?;
    waiter.lock.inner(token).owner.clone()
}

fn clear_blocked_if<H: PiHost>(waiter: &Arc<Waiter<H>>, token: &GraphToken) {
    let node = H::node(&waiter.task);
    if node
        .blocked(token)
        .is_some_and(|blocked| Arc::ptr_eq(&blocked, waiter))
    {
        node.set_blocked(None, token);
    }
}

fn unlink_donor<H: PiHost>(owner: &H::Task, waiter: &Waiter<H>, token: &GraphToken) {
    if !waiter.donor_link.is_linked() {
        return;
    }
    let key = waiter.key(token);
    let _ = H::node(owner).donors(token).find_mut(&key).remove();
}

fn link_donor<H: PiHost>(owner: &H::Task, waiter: &Arc<Waiter<H>>, token: &GraphToken) {
    if waiter.donor_link.is_linked() {
        return;
    }
    H::node(owner).donors(token).insert(waiter.clone());
}

/// Keeps exactly the lock's front waiter in the owner's donor tree.
fn refresh_top<H: PiHost>(lock: &LockState<H>, token: &GraphToken) {
    let inner = lock.inner(token);
    let front = inner.waiters.front().clone_pointer();
    let is_same = match (&inner.top, &front) {
        (Some(old), Some(new)) => Arc::ptr_eq(old, new),
        (None, None) => true,
        _ => false,
    };
    if is_same {
        return;
    }
    if let Some(old) = inner.top.take()
        && let Some(owner) = inner.owner.as_ref()
    {
        unlink_donor(owner, &old, token);
    }
    if let (Some(front), Some(owner)) = (front.as_ref(), inner.owner.as_ref()) {
        link_donor(owner, front, token);
    }
    inner.top = front;
}

/// Queues a new waiter and records what its task is blocked on.
pub(crate) fn enqueue<H: PiHost>(waiter: &Arc<Waiter<H>>, token: &GraphToken) {
    H::node(&waiter.task).set_blocked(Some(waiter.clone()), token);
    waiter.lock.inner(token).waiters.insert(waiter.clone());
    refresh_top(&waiter.lock, token);
}

/// Removes a queued waiter. The caller propagates from the returned owner.
pub(crate) fn dequeue<H: PiHost>(waiter: &Arc<Waiter<H>>, token: &GraphToken) -> Option<H::Task> {
    let lock = &waiter.lock;
    let key = waiter.key(token);
    let inner = lock.inner(token);
    if let Some(owner) = inner.owner.as_ref() {
        unlink_donor(owner, waiter, token);
    }
    if inner
        .top
        .as_ref()
        .is_some_and(|top| Arc::ptr_eq(top, waiter))
    {
        inner.top = None;
    }
    let _ = inner.waiters.find_mut(&key).remove();
    clear_blocked_if(waiter, token);
    refresh_top(lock, token);
    lock.inner(token).owner.clone()
}

/// What changed when a lock passed to its next owner.
pub(crate) struct Transfer<H: PiHost> {
    pub(crate) old_owner: Option<H::Task>,
    pub(crate) new_owner: Option<H::Task>,
    pub(crate) waker: Option<Waker>,
}

/// Gives the lock to its front waiter, or leaves it free with no waiters.
pub(crate) fn transfer<H: PiHost>(lock: &LockState<H>, token: &GraphToken) -> Transfer<H> {
    let inner = lock.inner(token);
    if let Some(top) = inner.top.take()
        && let Some(owner) = inner.owner.as_ref()
    {
        unlink_donor(owner, &top, token);
    }
    let old_owner = inner.owner.take();
    let next = inner.waiters.front_mut().remove();
    let mut waker = None;
    if let Some(next) = next.as_ref() {
        clear_blocked_if(next, token);
        waker = next.grant(token);
        inner.owner = Some(next.task.clone());
    }
    refresh_top(lock, token);
    Transfer {
        old_owner,
        new_owner: lock.inner(token).owner.clone(),
        waker,
    }
}

/// Re-sorts a queued waiter whose donation changed. Returns whether it did.
fn rekey<H: PiHost>(waiter: &Arc<Waiter<H>>, token: &GraphToken) -> bool {
    if !waiter.lock_link.is_linked() {
        return false;
    }
    let priority = H::donor_priority(&waiter.task);
    if waiter.key(token).priority() == priority {
        return false;
    }
    let _ = dequeue(waiter, token);
    waiter.set_priority(priority, token);
    enqueue(waiter, token);
    true
}

/// Walks the owner chain from `start`, applying each task's highest donation.
///
/// Stops where nothing changed, at a task that is not blocked, or at the
/// depth bound. The graph lock is dropped around every host boost call.
pub(crate) fn propagate<H: PiHost>(start: H::Task) {
    let mut current = start;
    for _ in 0..MAX_PI_DEPTH {
        let mut is_changed = false;
        loop {
            let boost = with_graph(|token| donor_boost::<H>(&current, token));
            is_changed |= H::set_boost(&current, boost);
            // A concurrent propagation may have submitted a newer donation.
            // Repair an obsolete write before any early return or chain walk.
            if with_graph(|token| donor_boost::<H>(&current, token)) == boost {
                break;
            }
        }
        let next = with_graph(|token| {
            let waiter = H::node(&current).blocked(token)?;
            let is_rekeyed = rekey(&waiter, token);
            if !is_changed && !is_rekeyed {
                return None;
            }
            let owner = waiter.lock.inner(token).owner.clone()?;
            if H::same_task(&owner, &current) {
                return None;
            }
            Some(owner)
        });
        let Some(owner) = next else {
            return;
        };
        current = owner;
    }
}

/// Re-sorts `task`'s pending wait after its own scheduling policy changed.
///
/// Call without the host's run-queue lock. Returns immediately when `task`
/// is not waiting on a priority-inheritance lock.
pub fn adjust_waiter_priority<H: PiHost>(task: &H::Task) {
    let owner = with_graph(|token| {
        let waiter = H::node(task).blocked(token)?;
        if !rekey(&waiter, token) {
            return None;
        }
        waiter.lock.inner(token).owner.clone()
    });
    if let Some(owner) = owner {
        propagate::<H>(owner);
    }
}

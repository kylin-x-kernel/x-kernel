// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Ownership of intrusive file-lock nodes and their shared waiting relations.
//!
//! An installed node has exactly one range owner: LockList or OwnedLock. Shared
//! Arc handles expose identity and waiting relations only. A Waiter never grants
//! range ownership, so its range stays immutable even while linked or notified.
//! These capabilities, rather than an unrelated inode guard argument, authorize
//! range access. All waiting links remain private and use BLOCKED_OWNERS.

use alloc::{
    collections::BTreeMap,
    sync::{Arc, Weak},
};
use core::{
    cell::UnsafeCell,
    future::poll_fn,
    task::{Poll, Waker},
};

use intrusive_collections::{LinkedList, LinkedListLink, intrusive_adapter};
use kerrno::{KError, KResult, LinuxError};
use ksync::{Mutex, MutexGuard, static_lock};
use ktask::future::{block_on, interruptible};

use super::{FileLock, FileLockOwner};

// Shared handles never authorize reading or mutating an installed range.
struct LockNode {
    owner: FileLockOwner,
    kind: super::FileLockType,
    pid: i32,
    bounds: UnsafeCell<(i64, i64)>,
    links: LinkedListLink,
    blocked: UnsafeCell<FileLockBlocked>,
}

struct FileLockBlocked {
    blocker: Option<Weak<LockNode>>,
    requests: LinkedList<LockAdapter>,
    wait: Option<Waker>,
}

// SAFETY: installed bounds have one non-cloneable owner (LockList/OwnedLock).
// List borrows prevent removal while being read; mutation requires &mut OwnedLock.
// Waiter bounds never change. Shared handles cannot obtain range ownership.
// All waiting links and registrations use the one private global mutex; inode
// and waiting membership are disjoint and only the corresponding owner changes
// links. Immutable owner/kind/pid remain readable during range mutation.
unsafe impl Sync for LockNode {}

intrusive_adapter!(LockAdapter = Arc<LockNode>: LockNode { links => LinkedListLink });

impl LockNode {
    fn new(record: FileLock) -> Self {
        Self {
            owner: record.owner,
            kind: record.kind,
            pid: record.pid,
            bounds: UnsafeCell::new(record.bounds),
            links: LinkedListLink::new(),
            blocked: UnsafeCell::new(FileLockBlocked {
                blocker: None,
                requests: LinkedList::new(LockAdapter::new()),
                wait: None,
            }),
        }
    }

    /// Copies the record without granting access to its shared storage.
    ///
    /// # Safety
    /// The caller owns the installed range, borrows its owning list, or knows this
    /// node is a waiter (whose range is immutable for its entire lifetime).
    unsafe fn record(&self) -> FileLock {
        // SAFETY: the caller establishes exclusive range ownership or immutable
        // waiter provenance. No shared reference to the cell escapes this copy.
        let bounds = unsafe { *self.bounds.get() };
        FileLock {
            owner: self.owner,
            kind: self.kind,
            pid: self.pid,
            bounds,
        }
    }

    fn state<'a>(
        &'a self,
        _guard: &'a mut MutexGuard<'_, BlockedOwners>,
    ) -> &'a mut FileLockBlocked {
        // SAFETY: every caller passes the guard of BLOCKED_OWNERS, the sole
        // lock protecting this cell. Borrowing the guard exclusively prevents
        // simultaneous references to any node's mutable linkage.
        unsafe { &mut *self.blocked.get() }
    }

    fn find(list: &LinkedList<LockAdapter>, matches: impl Fn(&Self) -> bool) -> Option<Arc<Self>> {
        let mut cursor = list.front();
        while let Some(lock) = cursor.get() {
            if matches(lock) {
                return cursor.clone_pointer();
            }
            cursor.move_next();
        }
        None
    }

    fn wake_blocked(&self, blocked: &mut MutexGuard<'_, BlockedOwners>, wake: &mut WakeQueue) {
        while let Some(waiter) = self.state(blocked).requests.pop_front() {
            waiter.state(blocked).blocker = None;
            remove_blocked_owner(blocked, &waiter);
            // The registration moves with its existing request node. A
            // pending notification keeps `wait` populated until dequeue, so
            // even a spurious poll cannot reuse these links prematurely.
            if waiter.state(blocked).wait.is_some() {
                wake.0.push_back(waiter);
            }
        }
    }

    fn move_blocked(&self, target: &Arc<Self>, blocked: &mut MutexGuard<'_, BlockedOwners>) {
        while let Some(waiter) = self.state(blocked).requests.pop_front() {
            waiter.state(blocked).blocker = Some(Arc::downgrade(target));
            target.state(blocked).requests.push_back(waiter);
        }
    }

    #[cfg(unittest)]
    pub(super) fn children_count(&self) -> usize {
        self.state(&mut BLOCKED_OWNERS.lock())
            .requests
            .iter()
            .count()
    }
}

/// The unique owner of an installed node outside a list. Never cloned or made
/// from a shared handle; global wait-tree readers cannot change its range.
pub(super) struct OwnedLock(Arc<LockNode>);

impl OwnedLock {
    pub(super) fn new(record: FileLock) -> Self {
        Self(Arc::new(LockNode::new(record)))
    }

    pub(super) fn record(&self) -> FileLock {
        // SAFETY: this unique owner is detached from every installed list;
        // wait-tree handles neither read nor modify installed ranges.
        unsafe { self.0.record() }
    }

    pub(super) fn set_bounds(&mut self, start: i64, end: i64) {
        // SAFETY: &mut self excludes all range readers through this owner.
        // The node is detached; shared handles access disjoint waiting state.
        unsafe { *self.0.bounds.get() = (start, end) };
    }

    pub(super) fn accept_waiters(&self, waiter: &Waiter) {
        waiter.0.move_blocked(&self.0, &mut BLOCKED_OWNERS.lock());
    }

    pub(super) fn detach_blocked(&mut self, wake: &mut WakeQueue) {
        // Weak parents prevent exclusive Arc access while descendants exist.
        // This is an ownership check, not an unsynchronized shared-list read.
        if let Some(node) = Arc::get_mut(&mut self.0)
            && node.blocked.get_mut().requests.is_empty()
        {
            return;
        }
        self.0.wake_blocked(&mut BLOCKED_OWNERS.lock(), wake);
    }
}

/// Owns installed nodes, exposing range copies under its own shared borrow.
/// Inode code stores this list inside FileLockLists's mutex. Only freshly built
/// or removed OwnedLock values can enter; shared blocker handles cannot.
pub(super) struct LockList(LinkedList<LockAdapter>);

impl LockList {
    pub(super) fn new() -> Self {
        Self(LinkedList::new(LockAdapter::new()))
    }

    #[cfg(unittest)]
    pub(super) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = FileLock> + '_ {
        self.0.iter().map(|node| {
            // SAFETY: the list owns the range. Its shared borrow excludes
            // removal into OwnedLock, the only capability permitting mutation.
            unsafe { node.record() }
        })
    }

    pub(super) fn find(&self, matches: impl Fn(&FileLock) -> bool) -> Option<Blocker> {
        LockNode::find(&self.0, |node| {
            // SAFETY: same owning-list borrow as iter(); no range reference escapes.
            matches(&unsafe { node.record() })
        })
        .map(Blocker)
    }

    pub(super) fn push_back(&mut self, lock: OwnedLock) {
        self.0.push_back(lock.0);
    }

    pub(super) fn pop_front(&mut self) -> Option<OwnedLock> {
        self.0.pop_front().map(OwnedLock)
    }

    pub(super) fn remove_owner(&mut self, owner: FileLockOwner, wake: &mut WakeQueue) {
        let mut cursor = self.0.front_mut();
        while let Some(node) = cursor.get() {
            if node.owner == owner {
                if let Some(node) = cursor.remove() {
                    OwnedLock(node).detach_blocked(wake);
                }
            } else {
                cursor.move_next();
            }
        }
    }

    #[cfg(unittest)]
    pub(super) fn has_no_waiters(&self) -> bool {
        let mut guard = BLOCKED_OWNERS.lock();
        self.0
            .iter()
            .all(|node| node.state(&mut guard).requests.is_empty())
    }
}

/// An immutable request plus its shared waiting identity. Cloning a waiter
/// never creates an installed-range owner or permits insertion in inode lists.
#[derive(Clone)]
pub(super) struct Waiter(Arc<LockNode>);

impl Waiter {
    pub(super) fn new(record: FileLock) -> Self {
        Self(Arc::new(LockNode::new(record)))
    }

    pub(super) fn move_blocked(&self, target: &Blocker) {
        self.0.move_blocked(&target.0, &mut BLOCKED_OWNERS.lock());
    }

    pub(super) fn block_on(&self, blocker: Blocker, wake: &mut WakeQueue) -> KResult<()> {
        let mut blocker = blocker.0;
        let mut blocked = BLOCKED_OWNERS.lock();
        // A pending notification still owns the intrusive links. Never allow
        // a second insertion to overwrite their recorded parent relationship.
        let state = self.0.state(&mut blocked);
        assert!(state.blocker.is_none() && state.wait.is_none());
        // A retry may meet a different blocker; descendants must retry too.
        self.0.wake_blocked(&mut blocked, wake);
        // SAFETY: only Waiter::new creates this handle; its range is immutable.
        let request = unsafe { self.0.record() };
        if has_deadlock(&mut blocked, &request, blocker.owner) {
            return Err(LinuxError::EDEADLK.into());
        }
        loop {
            let next = LockNode::find(&blocker.state(&mut blocked).requests, |other| {
                // SAFETY: only this method populates blocked requests, always
                // from a Waiter. Their ranges never become installed/mutable.
                request.conflicts_with(&unsafe { other.record() })
            });
            match next {
                Some(next) => blocker = next,
                None => break,
            }
        }
        self.0.state(&mut blocked).blocker = Some(Arc::downgrade(&blocker));
        blocker
            .state(&mut blocked)
            .requests
            .push_back(self.0.clone());
        // Indexing depends on the actual blocker, not detection eligibility.
        if blocker.owner.is_posix() {
            blocked
                .entry(self.0.owner)
                .or_default()
                .insert(Arc::as_ptr(&self.0).addr(), self.0.clone());
        }
        Ok(())
    }

    pub(super) fn cancel_waiter(&self, wake: &mut WakeQueue) {
        let discarded = {
            let mut blocked = BLOCKED_OWNERS.lock();
            let parent = self
                .0
                .state(&mut blocked)
                .blocker
                .take()
                .and_then(|weak| weak.upgrade());
            let discarded = if let Some(parent) = parent {
                // SAFETY: the blocker relation records membership in this
                // exact list, and the global guard serializes all list edits.
                let _ = unsafe {
                    parent
                        .state(&mut blocked)
                        .requests
                        .cursor_mut_from_ptr(Arc::as_ptr(&self.0))
                        .remove()
                };
                self.0.state(&mut blocked).wait.take()
            } else {
                // A detached request may already belong to a notification
                // list. Leave its registration for that list's consumer.
                None
            };
            remove_blocked_owner(&mut blocked, &self.0);
            self.0.wake_blocked(&mut blocked, wake);
            discarded
        };
        drop(discarded);
    }

    #[cfg(unittest)]
    pub(super) fn is_blocked(&self) -> bool {
        let mut guard = BLOCKED_OWNERS.lock();
        let blocked = self.0.state(&mut guard);
        blocked.blocker.is_some() || blocked.wait.is_some()
    }

    pub(super) fn poll_wait(&self, waker: &Waker) -> Poll<()> {
        // Clone and drop arbitrary wakers outside the wait-tree lock.
        let waker = waker.clone();
        let mut guard = BLOCKED_OWNERS.lock();
        let blocked = self.0.state(&mut guard);
        if blocked.blocker.is_none() && blocked.wait.is_none() {
            drop(guard);
            return Poll::Ready(());
        }
        let old = blocked.wait.replace(waker);
        drop(guard);
        drop(old);
        Poll::Pending
    }

    pub(super) fn wait_interruptible(&self) -> KResult<()> {
        block_on(interruptible(poll_fn(|cx| self.poll_wait(cx.waker())))).map_err(
            |error| -> KError {
                if error.is_signal() {
                    LinuxError::ERESTARTSYS.into()
                } else {
                    error.into()
                }
            },
        )
    }

    #[cfg(unittest)]
    pub(super) fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    #[cfg(unittest)]
    pub(super) fn is_blocked_by(&self, other: &Self) -> bool {
        self.parent_matches(&other.0)
    }

    #[cfg(unittest)]
    pub(super) fn is_blocked_by_node(&self, other: &Blocker) -> bool {
        self.parent_matches(&other.0)
    }

    #[cfg(unittest)]
    fn parent_matches(&self, other: &Arc<LockNode>) -> bool {
        self.0
            .state(&mut BLOCKED_OWNERS.lock())
            .blocker
            .as_ref()
            .is_some_and(|parent| Weak::ptr_eq(parent, &Arc::downgrade(other)))
    }
}

// The inner index distinguishes simultaneous requests by one files owner. It
// replaces Linux's owner hash bucket without a second intrusive ownership link.
type BlockedOwners = BTreeMap<FileLockOwner, BTreeMap<usize, Arc<LockNode>>>;
static_lock! {
    static BLOCKED_OWNERS: Mutex<BlockedOwners> = Mutex::new(BTreeMap::new());
}

fn remove_blocked_owner(blocked: &mut BlockedOwners, waiter: &Arc<LockNode>) {
    if let Some(requests) = blocked.get_mut(&waiter.owner) {
        requests.remove(&Arc::as_ptr(waiter).addr());
        if requests.is_empty() {
            blocked.remove(&waiter.owner);
        }
    }
}

fn has_deadlock(
    blocked: &mut MutexGuard<'_, BlockedOwners>,
    request: &FileLock,
    mut owner: FileLockOwner,
) -> bool {
    if !request.owner.is_posix() {
        return false;
    }
    // Match Linux's bounded owner-chain search, including its OFD exclusion.
    for _ in 0..=10 {
        let Some(mut waiter) = blocked
            .get(&owner)
            .and_then(|requests| requests.values().next())
            .cloned()
        else {
            return false;
        };
        // A waiter can depend on other waiters. Only the root's owner holds
        // the resource whose release will let this chain make progress.
        loop {
            let next = waiter
                .state(blocked)
                .blocker
                .as_ref()
                .and_then(Weak::upgrade);
            match next {
                Some(next) => waiter = next,
                None => break,
            }
        }
        if waiter.owner == request.owner {
            return true;
        }
        owner = waiter.owner;
    }
    false
}

/// Owns detached notification nodes until registration handoff completes.
/// Callers cannot insert arbitrary nodes or prematurely recycle their links.
pub(super) struct WakeQueue(LinkedList<LockAdapter>);
impl WakeQueue {
    pub(super) fn new() -> Self {
        Self(LinkedList::new(LockAdapter::new()))
    }

    #[cfg(unittest)]
    pub(super) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[cfg(unittest)]
    pub(super) fn len(&self) -> usize {
        self.0.iter().count()
    }

    pub(super) fn wake(&mut self) {
        while !self.0.is_empty() {
            let (waiter, waker) = {
                let mut blocked = BLOCKED_OWNERS.lock();
                let Some(waiter) = self.0.pop_front() else {
                    break;
                };
                let waker = waiter.state(&mut blocked).wait.take();
                (waiter, waker)
            };
            // Neither wake nor arbitrary Waker destruction holds either mutex.
            if let Some(waker) = waker {
                waker.wake();
            }
            drop(waiter);
        }
    }
}

impl Default for LockList {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(unittest)]
pub(super) fn has_blocked_owner(owner: FileLockOwner) -> bool {
    BLOCKED_OWNERS.lock().contains_key(&owner)
}
#[cfg(unittest)]
pub(super) fn wait_tree_unlocked() -> bool {
    BLOCKED_OWNERS.try_lock().is_some()
}
#[cfg(unittest)]
pub(super) fn hold_wait_tree() -> impl Sized {
    BLOCKED_OWNERS.lock()
}

/// Shared blocker identity, deliberately not an intrusive-list entry or range
/// owner. Hiding Arc<LockNode> prevents safe callers from linking it elsewhere.
pub(super) struct Blocker(Arc<LockNode>);

#[cfg(unittest)]
impl Blocker {
    pub(super) fn children_count(&self) -> usize {
        self.0.children_count()
    }

    pub(super) fn downgrade(&self) -> WeakBlocker {
        WeakBlocker(Arc::downgrade(&self.0))
    }

    pub(super) fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
#[cfg(unittest)]
pub(super) struct WeakBlocker(Weak<LockNode>);
#[cfg(unittest)]
impl WeakBlocker {
    pub(super) fn upgrade(&self) -> Option<Blocker> {
        self.0.upgrade().map(Blocker)
    }
}

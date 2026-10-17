// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared kernel PID/TID number allocation and namespace projections.
//!
//! Use [`PidHandle::allocate_in`] for a namespace or
//! [`allocate_root_pid_handle`] for the global root. Allocation does not publish
//! or schedule a task, and dropping a handle does not recycle its numbers.
//! Each handle also carries a [`PidLifecycleState`] that gates observer
//! visibility: a reserved identity is not lookup-visible until the task owner
//! marks it published, and an exiting identity remains visible until reaped.
//!
//! # Example
//!
//! ```
//! extern crate alloc;
//! use alloc::sync::Arc;
//!
//! use kidentity::{PidHandle, PidNamespace};
//! let root = Arc::new(PidNamespace::new_root());
//! let child = PidNamespace::new_child(&root);
//! let handle = PidHandle::allocate_in(&child).unwrap();
//! assert!(!handle.is_lookup_visible());
//! handle.mark_published();
//! assert!(handle.is_lookup_visible());
//! // A process in the root namespace observes the root projection; a process
//! // in the child namespace observes the child-side number.
//! assert_eq!(handle.visible_number(&child), Some(1));
//! assert_eq!(handle.visible_number(&root), Some(handle.root_nr()));
//! ```

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};

use kerrno::{KError, KResult};
use lazyinit::LazyInit;

/// PID namespace identity.
#[derive(Debug)]
pub struct PidNamespace {
    id: u64,
    parent: Option<Arc<PidNamespace>>,
    level: u32,
    next_nr: AtomicU32,
}

/// Kernel-wide monotonic PID-namespace identity allocator.
///
/// The identity is stable for the namespace's lifetime and is exported through
/// `/proc/<pid>/ns/pid` as the inode number, mirroring Linux's nsfs-backed
/// namespace inode. The counter starts at one; exhaustion is not checked.
static NEXT_PID_NS_ID: AtomicU32 = AtomicU32::new(1);

fn allocate_pid_ns_id() -> u64 {
    NEXT_PID_NS_ID.fetch_add(1, Ordering::Relaxed) as u64
}

impl PidNamespace {
    /// Creates the root PID namespace.
    pub fn new_root() -> Self {
        Self {
            id: allocate_pid_ns_id(),
            parent: None,
            level: 0,
            next_nr: AtomicU32::new(1),
        }
    }

    /// Creates a child PID namespace below `parent`.
    pub fn new_child(parent: &Arc<Self>) -> Arc<Self> {
        Arc::new(Self {
            id: allocate_pid_ns_id(),
            parent: Some(parent.clone()),
            level: parent.level + 1,
            next_nr: AtomicU32::new(1),
        })
    }

    /// Returns the stable namespace identity.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Returns the parent namespace, if any.
    pub fn parent(&self) -> Option<&Arc<PidNamespace>> {
        self.parent.as_ref()
    }

    /// Returns the namespace nesting level.
    pub fn level(&self) -> u32 {
        self.level
    }

    /// Returns `true` if this is the root PID namespace.
    pub fn is_root(&self) -> bool {
        self.parent.is_none()
    }

    fn allocate_nr(&self) -> KResult<u32> {
        self.next_nr
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |next| {
                next.checked_add(1)
            })
            .map_err(|_overflowed| KError::WouldBlock)
    }
}

/// Lifecycle phase of a [`PidHandle`] identity.
///
/// The phases mirror Linux's pid lifecycle discipline: a number is reserved
/// from the allocator before the task is published (so the number cannot be
/// observed half-configured), stays lookup-visible through its exit path until
/// every waiter has been satisfied, and only then is recycled. X-Kernel's
/// allocator is monotonically increasing, so numbers are never reused while the
/// handle is alive; this state only controls *observer visibility*.
///
/// | state | lookup-visible | meaning |
/// | --- | --- | --- |
/// | `Reserved` | no | allocated, task not yet published |
/// | `Published` | yes | task live, number is authoritative |
/// | `Retiring` | yes | exit in progress; waiters may still resolve it |
/// | `Reclaimed` | no | reaped; identity no longer resolvable |
///
/// The transition table is closed: any other transition is rejected by
/// [`PidLifecycleState::try_transition`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum PidLifecycleState {
    /// Number allocated but not yet published.
    Reserved  = 0,
    /// Task is live and the number is authoritative.
    Published = 1,
    /// Exit has begun; waiters may still resolve the identity.
    Retiring  = 2,
    /// Reaped; the identity is no longer lookup-visible.
    Reclaimed = 3,
}

impl PidLifecycleState {
    /// Returns `true` when lookups may observe an identity in this state.
    ///
    /// Both `Published` and `Retiring` remain resolvable, aligning with Linux's
    /// `pid_allocated` semantics that keep an exiting task findable until its
    /// exit has fully completed.
    pub const fn is_lookup_visible(self) -> bool {
        matches!(self, Self::Published | Self::Retiring)
    }

    /// Returns the valid successor state for `target`, if the transition is legal.
    ///
    /// Valid transitions:
    /// - `Reserved -> Published` (publication commits);
    /// - `Reserved -> Reclaimed` (allocation aborted before publication);
    /// - `Published -> Retiring` (exit begins);
    /// - `Published -> Reclaimed` (reaped with no waiters);
    /// - `Retiring -> Reclaimed` (exit completes for all waiters).
    pub const fn try_transition(self, target: Self) -> Option<Self> {
        // `target` is used below; the explicit arm list keeps the allowed
        // transition graph auditable in one place.
        match (self, target) {
            (Self::Reserved, Self::Published | Self::Reclaimed) => Some(target),
            (Self::Published, Self::Retiring | Self::Reclaimed) => Some(target),
            (Self::Retiring, Self::Reclaimed) => Some(target),
            _ => None,
        }
    }
}

/// State encoded in the `u8` that backs [`PidHandle`]'s lifecycle atomic.
fn state_from_u8(value: u8) -> Option<PidLifecycleState> {
    match value {
        0 => Some(PidLifecycleState::Reserved),
        1 => Some(PidLifecycleState::Published),
        2 => Some(PidLifecycleState::Retiring),
        3 => Some(PidLifecycleState::Reclaimed),
        _ => None,
    }
}

/// A namespace-scoped task number.
#[derive(Debug, Clone)]
pub struct Upid {
    nr: u32,
    ns: Arc<PidNamespace>,
}

impl Upid {
    /// Returns the number within its namespace.
    pub fn nr(&self) -> u32 {
        self.nr
    }

    /// Returns the owning namespace.
    pub fn ns(&self) -> &Arc<PidNamespace> {
        &self.ns
    }
}

/// Stable, never-reused identity token for a task/thread.
///
/// Allocated once when the task is created and unchanged for its whole life,
/// including across an `exec` leader takeover that moves the numeric TID.
/// Registries that must not be re-indexed when a number moves key on this
/// token; the numeric directory remains the only number-indexed table.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct TaskIdentityToken(u64);

impl TaskIdentityToken {
    /// Allocates a fresh, process-lifetime-unique token.
    pub fn allocate() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }

    /// Returns the reserved token for runtimes without a user identity.
    ///
    /// Never handed out by [`Self::allocate`]; only test or kernel-only
    /// runtimes that never join a token-indexed registry use it.
    pub const fn unknown() -> Self {
        Self(0)
    }

    /// Returns the raw token value for tracing and diagnostics.
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

/// Shared task-number handle, analogous to Linux `struct pid`.
#[derive(Debug)]
pub struct PidHandle {
    numbers: Vec<Upid>,
    lifecycle: AtomicU8,
}

impl PidHandle {
    /// Allocates a new handle visible in `active_ns` and all its ancestors.
    ///
    /// The handle starts in the [`PidLifecycleState::Reserved`] state and must
    /// be transitioned to `Published` by the task owner before it becomes
    /// lookup-visible.
    ///
    /// # Errors
    ///
    /// Returns `KError::WouldBlock` if a namespace counter cannot increment.
    /// Numbers already consumed in other namespaces are not rolled back.
    pub fn allocate_in(active_ns: &Arc<PidNamespace>) -> KResult<Arc<Self>> {
        let mut chain = Vec::new();
        let mut cursor = Some(active_ns.clone());
        while let Some(ns) = cursor {
            chain.push(ns.clone());
            cursor = ns.parent().cloned();
        }

        let mut numbers = Vec::with_capacity(chain.len());
        for ns in chain {
            numbers.push(Upid {
                nr: ns.allocate_nr()?,
                ns,
            });
        }

        Ok(Arc::new(Self {
            numbers,
            lifecycle: AtomicU8::new(PidLifecycleState::Reserved as u8),
        }))
    }

    /// Creates a root-only handle with a fixed number.
    ///
    /// This does not reserve the number in the root allocator or check uniqueness.
    /// The caller must prevent conflicting numeric registrations. The resulting
    /// identity is immediately [`PidLifecycleState::Published`]; fixed identities
    /// model pre-existing or kernel-owned tasks (init, test fixtures) that are
    /// live from construction.
    pub fn fixed_root(nr: u32) -> Arc<Self> {
        Arc::new(Self {
            numbers: alloc::vec![Upid {
                nr,
                ns: root_pid_namespace().clone(),
            }],
            lifecycle: AtomicU8::new(PidLifecycleState::Published as u8),
        })
    }

    /// Returns the root-visible number.
    ///
    /// # Panics
    ///
    /// Panics if the handle has no root projection; both public constructors
    /// maintain that invariant.
    pub fn root_nr(&self) -> u32 {
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: Both PID-handle constructors install \
                      a root-visible number alongside any nested namespace projections"
        )]
        self.numbers
            .iter()
            .find(|upid| upid.ns.is_root())
            .expect("pid handle must always have a root-visible number")
            .nr
    }

    /// Returns the number visible in `ns`, if present.
    ///
    /// A different root namespace deliberately receives the root projection too;
    /// `None` only describes a missing non-root projection. This is not an
    /// authorization or namespace-equality predicate.
    pub fn nr_in(&self, ns: &Arc<PidNamespace>) -> Option<u32> {
        self.numbers
            .iter()
            .find(|upid| Arc::ptr_eq(upid.ns(), ns))
            .map(Upid::nr)
            .or_else(|| ns.is_root().then(|| self.root_nr()))
    }

    /// Returns the number in the namespace the task lives in: the deepest
    /// (most nested) projection installed at creation time, i.e. the
    /// `task_active_pid_ns(current)` view that `getpid(2)` reports.
    ///
    /// # Panics
    ///
    /// Panics if the handle carries no projection; both public constructors
    /// maintain that invariant.
    pub fn active_nr(&self) -> u32 {
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: Both PID-handle constructors install \
                      at least the active-namespace projection"
        )]
        self.numbers
            .first()
            .expect("pid handle must always carry its active-namespace number")
            .nr
    }

    /// Returns the namespace the task lives in — the deepest projection's
    /// owner, analogous to Linux `task_active_pid_ns`. `setns(CLONE_NEWPID)`
    /// changes `pid_ns_for_children` instead, so this stays fixed for the
    /// handle's lifetime.
    ///
    /// # Panics
    ///
    /// Panics if the handle carries no projection; both public constructors
    /// maintain that invariant.
    pub fn active_ns(&self) -> &Arc<PidNamespace> {
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: Both PID-handle constructors install \
                      at least the active-namespace projection"
        )]
        self.numbers
            .first()
            .map(|upid| upid.ns())
            .expect("pid handle must always carry its active-namespace number")
    }

    /// Returns the number an observer in `observer` sees for this identity.
    ///
    /// The observer walk starts at `observer` and climbs to its root, returning
    /// the number at the deepest-common ancestor namespace that holds a
    /// projection for this identity. Because every handle carries a root
    /// projection and every namespace's ancestor chain reaches the root, an
    /// observer inside the identity's own namespace hierarchy always sees a
    /// number. An observer rooted in an unrelated namespace tree receives
    /// `None`, matching Linux's out-of-hierarchy invisibility.
    pub fn visible_number(&self, observer: &Arc<PidNamespace>) -> Option<u32> {
        let mut cursor = Some(observer.clone());
        while let Some(ns) = cursor {
            if let Some(upid) = self.numbers.iter().find(|upid| Arc::ptr_eq(upid.ns(), &ns)) {
                return Some(upid.nr());
            }
            cursor = ns.parent().cloned();
        }
        None
    }

    /// Returns the number this identity holds in exactly `observer`.
    ///
    /// Unlike [`Self::visible_number`], the projection is not walked to an
    /// ancestor namespace: `None` is returned when the identity has no number
    /// allocated in `observer` itself.
    pub fn number_in(&self, observer: &Arc<PidNamespace>) -> Option<u32> {
        self.numbers
            .iter()
            .find(|upid| Arc::ptr_eq(upid.ns(), observer))
            .map(Upid::nr)
    }

    /// Returns the current lifecycle state.
    pub fn lifecycle_state(&self) -> PidLifecycleState {
        state_from_u8(self.lifecycle.load(Ordering::Acquire)).unwrap_or(PidLifecycleState::Reserved)
    }

    /// Returns `true` when lookups may resolve this identity.
    ///
    /// Reserved and reclaimed identities are filtered out, preventing a
    /// half-published or already-recycled task from being discovered.
    pub fn is_lookup_visible(&self) -> bool {
        self.lifecycle_state().is_lookup_visible()
    }

    /// Marks this identity as published; returns `false` if the current state
    /// does not permit publication.
    pub fn mark_published(&self) -> bool {
        self.transition(PidLifecycleState::Published)
    }

    /// Marks this identity as entering its exit path; returns `false` if the
    /// current state does not permit retiring.
    pub fn mark_retiring(&self) -> bool {
        self.transition(PidLifecycleState::Retiring)
    }

    /// Marks this identity as reclaimed; returns `false` if the current state
    /// does not permit reclamation.
    pub fn mark_reclaimed(&self) -> bool {
        self.transition(PidLifecycleState::Reclaimed)
    }

    /// Attempts a single lifecycle transition.
    ///
    /// The compare-exchange is ordered (`AcqRel`/`Acquire`) so the visible
    /// "published" observation synchronizes with the publication writes that
    /// precede it, and the "reclaimed" observation synchronizes with the exit
    /// completion writes that precede it.
    fn transition(&self, target: PidLifecycleState) -> bool {
        let mut current = self.lifecycle.load(Ordering::Acquire);
        loop {
            let Some(from) = state_from_u8(current) else {
                return false;
            };
            let Some(next) = PidLifecycleState::try_transition(from, target) else {
                return false;
            };
            match self.lifecycle.compare_exchange_weak(
                current,
                next as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(observed) => current = observed,
            }
        }
    }

    /// Returns the namespace-number vector.
    pub fn numbers(&self) -> &[Upid] {
        &self.numbers
    }
}

static ROOT_PID_NS: LazyInit<Arc<PidNamespace>> = LazyInit::new();

/// Returns the shared root PID namespace.
///
/// # Panics
///
/// Panics if the root remains uninitialized after `call_once`, which would
/// violate the lazy-initialization contract.
pub fn root_pid_namespace() -> &'static Arc<PidNamespace> {
    ROOT_PID_NS.call_once(|| Arc::new(PidNamespace::new_root()));
    #[expect(
        clippy::unwrap_used,
        reason = "CG-021-no-panic-on-reachable-failure: call_once completes root namespace \
                  initialization before get reads it"
    )]
    ROOT_PID_NS.get().unwrap()
}

/// Allocates a root-visible PID/TID handle.
///
/// # Errors
///
/// Returns `KError::WouldBlock` if a namespace counter cannot increment.
/// Numbers already consumed in other namespaces are not rolled back.
pub fn allocate_root_pid_handle() -> KResult<Arc<PidHandle>> {
    PidHandle::allocate_in(root_pid_namespace())
}

unittest::path_mod_test!(tests, "../ut-tests/lib.rs");

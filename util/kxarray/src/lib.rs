// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Sparse usize-indexed ownership with lockless, RCU-scoped borrowing.
//!
//! `XArray<Box<T>>` owns each inserted object; `XArray<Arc<T>>` owns one strong
//! reference and also supports explicit strong-reference acquisition. Insertion
//! transfers ownership only on success. Reads allocate nothing and never take
//! the writer lock. Replacement retires the displaced owner through RCU without
//! waiting for readers. Removal withdraws the association and retires its owner
//! through the same callback path.
//!
//! Insertion is an unsafe adoption boundary: Send/Sync cannot certify that a
//! value's destructor is suitable for RCU callback execution. Callers must audit
//! the full release chain, including nested fields and last-Arc destruction.
//!
//! # Usage
//!
//! The following sequence runs in initialized, watched task context with the
//! permanent RCU service available. Read scopes must not sleep or voluntarily
//! yield. Updates run outside scopes and scheduler/RCU bookkeeping locks.
//! Successful replacement/removal publishes a change; it does not mean old
//! objects have been destroyed. Stored data needs its own mutation protocol.
//!
//! ```no_run
//! extern crate alloc;
//! use alloc::boxed::Box;
//!
//! let entries = kxarray::XArray::<Box<usize>>::new();
//! // SAFETY: initialized task context, primitive payload and the kernel's
//! // nonblocking allocator release; no callback-needed locks are held.
//! let inserted = unsafe { entries.insert(42, Box::new(7)) };
//! assert!(inserted.is_ok());
//! let copied = krcu::with_read(|scope| entries.read(scope, 42).copied());
//! assert_eq!(copied, Some(7));
//! // SAFETY: same audited primitive release chain; eligible task outside
//! // read scopes/locks, with the permanent RCU service remaining available.
//! assert!(unsafe { entries.set(42, Box::new(9)) }.is_ok());
//! assert_eq!(
//!     krcu::with_read(|scope| entries.read(scope, 42).copied()),
//!     Some(9)
//! );
//! // SAFETY: eligible task and live RCU service, no callback-required lock.
//! assert_eq!(unsafe { entries.remove(42) }, Ok(true));
//! assert_eq!(
//!     krcu::with_read(|scope| entries.read(scope, 42).copied()),
//!     None
//! );
//! drop(entries); // Retired callbacks do not borrow the array.
//! ```
//!
//! With Arc, an explicitly acquired owner can outlive both the read scope and
//! the association. The caller is responsible for that owner's final Drop:
//!
//! ```no_run
//! extern crate alloc;
//! use alloc::sync::Arc;
//!
//! let entries = kxarray::XArray::<Arc<usize>>::new();
//! // SAFETY: allocation-permitted task; usize and Arc storage have a
//! // nonblocking release chain, including last-reference destruction.
//! assert!(unsafe { entries.insert(5, Arc::new(11)) }.is_ok());
//! let retained = krcu::with_read(|scope| entries.get_owned(scope, 5));
//! // SAFETY: initialized watched task outside scopes/locks, permanent RCU service.
//! assert_eq!(unsafe { entries.remove(5) }, Ok(true));
//! drop(entries);
//! assert_eq!(retained.as_deref(), Some(&11));
//! drop(retained); // This need not be the last Arc: retirement may still be pending.
//! ```
//!
//! Before RCU service readiness, new insertions and root growth are permitted
//! once allocation and local IRQ/per-CPU prerequisites exist. Replacement and
//! removal need runtime admission. There is no per-array callback drain; use
//! the system RCU drain contract separately when shutdown needs completion.
#![no_std]

extern crate alloc;

use alloc::{boxed::Box, sync::Arc};
use core::{
    marker::PhantomData,
    sync::atomic::{AtomicPtr, Ordering},
};

use kerrno::KError;
use krcu::ReadScope;
use kspin::SpinNoPreempt;

mod node;
mod owner;
mod retirement;

use node::{MAX_HEIGHT, Node, PreparedNodes, height_for};
pub use owner::EntryOwner;
use retirement::Retirement;

/// An owning sparse index supporting concurrent insertion and scoped reads.
///
/// Exclusive destruction releases all still-stored owners without waiting for
/// a grace period: safe borrows cannot outlive the array. Drop must run in a
/// context supporting allocator deallocation and the admitted values' release,
/// not NMI. Shared payload mutation remains the caller's responsibility.
///
/// Shared mutation must not allow shortening the carrier's lifetime:
///
/// ```compile_fail
/// fn inject<'short>(array: &kxarray::XArray<Box<&'static usize>>, value: &'short usize) {
///     let narrowed: &kxarray::XArray<Box<&'short usize>> = array;
///     // SAFETY: primitive reference payload has no blocking destructor.
///     let _ = unsafe { narrowed.insert(0, Box::new(value)) };
/// }
/// ```
pub struct XArray<P: EntryOwner> {
    root: AtomicPtr<Node>,
    writers: SpinNoPreempt<()>,
    owner: PhantomData<P>,
    // Shared insertion must not shorten lifetimes within the owning carrier.
    invariant: PhantomData<fn(P) -> P>,
    #[cfg(unittest)]
    preparation_hook: SpinNoPreempt<Option<tests::PreparationHook<P>>>,
    #[cfg(unittest)]
    publication_hook: SpinNoPreempt<Option<tests::PublicationHook<P>>>,
}

impl<P: EntryOwner> Default for XArray<P> {
    fn default() -> Self {
        Self::new()
    }
}

impl<P: EntryOwner> XArray<P> {
    /// Creates an empty index without allocation or RCU service readiness.
    pub const fn new() -> Self {
        Self {
            root: AtomicPtr::new(core::ptr::null_mut()),
            writers: SpinNoPreempt::new(()),
            owner: PhantomData,
            invariant: PhantomData,
            #[cfg(unittest)]
            preparation_hook: SpinNoPreempt::new(None),
            #[cfg(unittest)]
            publication_hook: SpinNoPreempt::new(None),
        }
    }

    /// Borrows an entry for no longer than both the array and RCU scope.
    ///
    /// No allocation, writer lock or reference-count increment is performed.
    /// Concurrent insertion may be observed before or after publication.
    ///
    /// ```compile_fail
    /// let array = kxarray::XArray::<Box<usize>>::new();
    /// let escaped = krcu::with_read(|scope| array.read(scope, 0));
    /// println!("{escaped:?}");
    /// ```
    ///
    /// The array must also remain alive for the entire borrow:
    ///
    /// ```compile_fail,E0505
    /// let array = kxarray::XArray::<Box<usize>>::new();
    /// krcu::with_read(|scope| {
    ///     let borrowed = array.read(scope, 0);
    ///     drop(array);
    ///     println!("{borrowed:?}");
    /// });
    /// ```
    pub fn read<'a>(&'a self, _scope: &'a ReadScope<'_>, index: usize) -> Option<&'a P::Target> {
        // SAFETY: the caller's live RCU scope protects every traversed node.
        let raw = unsafe { self.lookup(index) };
        // SAFETY: lookup follows initialized atomic links owned by this array.
        // The real RCU scope protects both detached nodes and
        // any displaced payload until this borrowed reference can no longer be used.
        unsafe { raw.cast::<P::Target>().as_ref() }
    }

    /// Inserts only if absent, returning the offered owner unchanged on conflict.
    ///
    /// Nodes are prepared outside the writer lock and the path is rechecked.
    /// Allocation uses the kernel's infallible allocation/OOM policy. Initial
    /// insertion and root growth do not require a running reclamation service.
    ///
    /// # Errors
    /// Returns `Err(value)` only when the key is already occupied, preserving
    /// the offered ownership and leaving the existing association unchanged.
    ///
    /// # Safety
    /// Call from an allocation-permitted task or early-boot context, with local
    /// IRQ/per-CPU primitives initialized; never from IRQ, softirq or NMI.
    /// The offered owner's complete release chain must not block or unwind and
    /// must be valid in ordinary RCU callback context, including nested values,
    /// last strong-reference destruction and allocator deallocation. This is an
    /// adoption contract, not a consequence of Send/Sync. Rejected ownership
    /// remains the caller's responsibility.
    pub unsafe fn insert(&self, index: usize, value: P) -> Result<(), P> {
        let mut prepared = PreparedNodes::new();
        loop {
            let guard = self.writers.lock();
            let Some(needed) = self.nodes_needed(index) else {
                drop(guard);
                return Err(value);
            };
            if prepared.len < needed {
                drop(guard);
                prepared.prepare(needed);
                continue;
            }
            self.publish(index, value, &mut prepared);
            drop(guard);
            return Ok(());
        }
    }

    /// Inserts or replaces an association without waiting for old readers.
    ///
    /// Replacement atomically publishes the new owner and retires only this
    /// array's old ownership after a grace period. It never returns the old
    /// owner. Detached callbacks do not borrow the array and may finish before
    /// this call returns. Missing keys may be inserted before RCU starts.
    /// Owners must be `'static`: asynchronous release cannot depend on a
    /// caller's stack lifetime even after the array itself has been destroyed.
    ///
    /// # Errors
    /// Replacement returns `(error, value)` unchanged if RCU preflight rejects
    /// service readiness or caller context. Publication is unchanged on error.
    /// Allocation follows the infallible kernel OOM policy.
    ///
    /// # Safety
    /// Uphold all of [`Self::insert`]'s context and full release-chain obligations.
    /// Hold no scheduler/RCU bookkeeping lock. If initialized, keep the permanent
    /// RCU service and participating CPUs available through callback execution.
    /// Preflight checks watched, sleepable task context outside read scopes
    /// before permitting replacement; failure returns the input unchanged.
    pub unsafe fn set(&self, index: usize, value: P) -> Result<(), (KError, P)>
    where
        P: 'static,
    {
        let mut prepared = PreparedNodes::new();
        let mut retirement = None;
        loop {
            let guard = self.writers.lock();
            if let Some(needed) = self.nodes_needed(index) {
                if prepared.len < needed {
                    drop(guard);
                    prepared.prepare(needed);
                    #[cfg(unittest)]
                    tests::after_set_preparation(self, index);
                    continue;
                }
                self.publish(index, value, &mut prepared);
                drop(guard);
                return Ok(());
            }
            if let Some(record) = retirement.take() {
                // nodes_needed observed an occupied leaf under this same lock;
                // no intervening writer can remove its path or association.
                #[expect(
                    clippy::expect_used,
                    reason = "CG-021-no-panic-on-reachable-failure: nodes_needed verified an \
                              occupied leaf under the same writer lock"
                )]
                // SAFETY: writer exclusion retains all nodes through the swap.
                let slot = unsafe { self.lookup_slot(index) }.expect("occupied radix path");
                let old = slot.swap(value.into_raw(), Ordering::AcqRel);
                drop(guard);
                // SAFETY: preflight succeeded before preparing this record.
                // The swap withdrew exactly one certified owner. No user code
                // or explicit context transition intervenes before submission.
                unsafe { Retirement::<P>::submit(record, old) };
                return Ok(());
            }
            drop(guard);
            if let Err(error) = krcu::preflight_callback_submission() {
                return Err((error, value));
            }
            retirement = Some(Retirement::<P>::prepare());
            #[cfg(unittest)]
            tests::after_set_preparation(self, index);
            // Reinspect after allocation: never use a path retained across unlock.
        }
    }

    /// Removes an association without waiting for readers or returning its owner.
    ///
    /// Returns whether this call removed an entry. Old borrows remain valid until
    /// their scopes end; the callback releases only the array's ownership.
    /// Empty branches are detached and released after a grace period. Removing
    /// the last entry clears the root; nonempty roots are not compacted.
    ///
    /// # Errors
    /// Returns the RCU preflight error without changing the association when
    /// service readiness or caller context is unsuitable. Even a missing-key
    /// removal requires successful preflight. Allocation follows kernel OOM policy.
    ///
    /// # Safety
    /// Uphold [`Self::set`]'s execution-context and permanent-service obligations.
    /// Stored owners have already been admitted under the insertion release
    /// contract. No scheduler/RCU bookkeeping lock may be held.
    pub unsafe fn remove(&self, index: usize) -> Result<bool, KError>
    where
        P: 'static,
    {
        krcu::preflight_callback_submission()?;
        let mut retirement = None;
        loop {
            let guard = self.writers.lock();
            // SAFETY: the writer lock retains the path through this iteration.
            let Some(slot) = (unsafe { self.lookup_slot(index) }) else {
                drop(guard);
                return Ok(false);
            };
            if slot.load(Ordering::Acquire).is_null() {
                drop(guard);
                return Ok(false);
            }
            if let Some(record) = retirement.take() {
                // SAFETY: this guard spans the occupied-slot check above and
                // withdrawal, so no writer can invalidate the verified path.
                let (old, detached) = unsafe { self.withdraw(index) };
                drop(guard);
                // SAFETY: preflight preceded withdrawal, and the writer lock
                // serialized inspection and removal of exactly one owner.
                // No explicit context transition precedes unlocked submission.
                unsafe { Retirement::<P>::submit(record, old) };
                for raw in detached.into_iter().filter(|raw| !raw.is_null()) {
                    // SAFETY: each empty node was detached under this lock.
                    // The local array, not any submitted node, supplies the next
                    // pointer, so immediate callbacks cannot invalidate iteration.
                    unsafe { Node::retire(raw) };
                }
                return Ok(true);
            }
            drop(guard);
            retirement = Some(Retirement::<P>::prepare());
            // Another writer may remove or replace the entry during preparation.
            // Discard the old path and inspect the current association again.
        }
    }

    /// # Safety
    /// Hold an RCU scope, writer lock, or exclude all destructive mutations
    /// throughout traversal and use of the returned pointer.
    unsafe fn lookup(&self, index: usize) -> *mut () {
        // SAFETY: caller retains traversal and payload lifetime protection.
        unsafe { self.lookup_slot(index) }
            .map_or(core::ptr::null_mut(), |slot| slot.load(Ordering::Acquire))
    }

    /// # Safety
    /// Hold an RCU scope, writer lock, or exclude destructive mutation for the
    /// entire traversal and every use of the returned slot reference.
    unsafe fn lookup_slot(&self, index: usize) -> Option<&AtomicPtr<()>> {
        let mut raw = self.root.load(Ordering::Acquire);
        if raw.is_null() {
            return None;
        }
        // SAFETY: caller protection retains the initialized root even if detached.
        // Only the root covers the whole index; children consume lower digits.
        if !unsafe { &*raw }.covers(index) {
            return None;
        }
        while !raw.is_null() {
            // SAFETY: caller protection retains even detached nodes through this
            // traversal; only shared references are formed.
            let node = unsafe { &*raw };
            if node.height == 1 {
                return Some(node.slot(index));
            }
            raw = node.slot(index).load(Ordering::Acquire).cast();
        }
        None
    }

    /// Stack storage is bounded by key width, not entry count or callback timing.
    ///
    /// # Safety
    /// Hold this array's writer lock throughout the call. The key must have a
    /// nonempty payload, established while holding that same lock or by exclusive
    /// access before acquiring it. Retain the returned payload and detached nodes
    /// until all readers that could have observed them finish.
    unsafe fn withdraw(&self, index: usize) -> (*mut (), [*mut Node; MAX_HEIGHT as usize]) {
        let mut path = [core::ptr::null_mut::<Node>(); MAX_HEIGHT as usize];
        let mut depth = 0;
        let mut raw = self.root.load(Ordering::Acquire);
        let old = loop {
            path[depth] = raw;
            depth += 1;
            // SAFETY: the occupied path was verified under the same writer lock.
            let node = unsafe { &*raw };
            if node.height == 1 {
                break node.remove_slot(index);
            }
            raw = node.slot(index).load(Ordering::Acquire).cast();
        };
        let mut detached = [core::ptr::null_mut(); MAX_HEIGHT as usize];
        for level in (0..depth).rev() {
            // SAFETY: writer exclusion retains all nodes saved in this path.
            let node = unsafe { &*path[level] };
            if !node.is_empty() {
                break;
            }
            if level == 0 {
                self.root.store(core::ptr::null_mut(), Ordering::Release);
            } else {
                // SAFETY: the parent remains live and owns this child until the
                // following removal. No callback is submitted under the lock.
                let parent = unsafe { &*path[level - 1] };
                parent.remove_slot(index);
            }
            detached[level] = path[level];
        }
        (old, detached)
    }

    // Called only under writers; no raw path escapes the critical section.
    fn nodes_needed(&self, index: usize) -> Option<usize> {
        let mut raw = self.root.load(Ordering::Acquire);
        if raw.is_null() {
            return Some(height_for(index) as usize);
        }
        // SAFETY: the writer lock stabilizes the array-owned root.
        let root = unsafe { &*raw };
        if !root.covers(index) {
            let height = height_for(index);
            // An uncovered key needs a taller root. Reserve H-R ancestors plus
            // H-1 nodes for its new downward path, separate from the old root.
            debug_assert!(root.height < height);
            return Some((height - root.height + height - 1) as usize);
        }
        loop {
            // SAFETY: writers serializes traversal of this live owned graph.
            let node = unsafe { &*raw };
            let entry = node.slot(index).load(Ordering::Acquire);
            if node.height == 1 {
                return entry.is_null().then_some(0);
            }
            if entry.is_null() {
                return Some((node.height - 1) as usize);
            }
            raw = entry.cast();
        }
    }

    fn publish(&self, index: usize, value: P, prepared: &mut PreparedNodes) {
        let mut root = self.root.load(Ordering::Acquire);
        let height = height_for(index);
        let mut current_height = if root.is_null() {
            0
        } else {
            // SAFETY: writers retains the live initialized root.
            unsafe { (*root).height }
        };
        if current_height == 0 {
            root = prepared.take(height);
            current_height = height;
        }
        while current_height < height {
            let parent = prepared.take(current_height + 1);
            // SAFETY: parent is private, initialized storage; attaching the old
            // root transfers its graph ownership without reconstructing a Box.
            unsafe { (*parent).insert_slot(0, root.cast()) };
            root = parent;
            current_height += 1;
        }
        let mut raw = root;
        loop {
            // SAFETY: every traversed node is initialized and retained. Even
            // under the writer lock we never create &mut to a published node.
            let node = unsafe { &*raw };
            let slot = node.slot(index);
            if node.height == 1 {
                node.insert_slot(index, value.into_raw());
                break;
            }
            let mut child = slot.load(Ordering::Acquire).cast::<Node>();
            if child.is_null() {
                child = prepared.take(node.height - 1);
                node.insert_slot(index, child.cast());
                #[cfg(unittest)]
                tests::after_child_publication(self, index);
            }
            raw = child;
        }
        self.root.store(root, Ordering::Release);
    }
}

impl<T: Send + Sync> XArray<Arc<T>> {
    /// Acquires an independent strong reference while the stored owner is protected.
    ///
    /// Unlike a borrowed entry, this owner may outlive the scope and array:
    ///
    /// ```no_run
    /// let array = kxarray::XArray::<std::sync::Arc<usize>>::new();
    /// let retained = krcu::with_read(|scope| array.get_owned(scope, 0));
    /// drop(array);
    /// drop(retained);
    /// ```
    pub fn get_owned(&self, _scope: &ReadScope<'_>, index: usize) -> Option<Arc<T>> {
        // SAFETY: the caller's read scope protects nodes and the stored Arc owner.
        let raw = unsafe { self.lookup(index) }.cast::<T>();
        if raw.is_null() {
            return None;
        }
        // SAFETY: the read scope and array borrow protect the stored strong
        // owner; raw retains the original allocation's provenance.
        unsafe { Arc::increment_strong_count(raw) };
        // SAFETY: consumes only the newly acquired strong reference, not the
        // array's owner; this pointer comes from Arc::into_raw in the carrier.
        Some(unsafe { Arc::from_raw(raw) })
    }
}

impl<P: EntryOwner> Drop for XArray<P> {
    fn drop(&mut self) {
        let root = *self.root.get_mut();
        if !root.is_null() {
            // SAFETY: exclusive destruction excludes all operations/borrows;
            // downward ownership has no sharing or cycles between nodes.
            unsafe { destroy_tree::<P>(root) };
        }
    }
}

/// # Safety
/// Owns the complete downward graph; no reader or other owner may access its
/// nodes or borrow its stored payloads. Each slot transfers exactly one owner.
unsafe fn destroy_tree<P: EntryOwner>(raw: *mut Node) {
    // SAFETY: exclusive graph ownership permits reconstructing this node Box.
    let node = unsafe { Box::from_raw(raw) };
    for slot in &node.slots {
        let entry = slot.load(Ordering::Relaxed);
        if entry.is_null() {
            continue;
        }
        if node.height == 1 {
            // SAFETY: this leaf retained one owner of P and all its borrows ended.
            unsafe { P::drop_raw(entry) };
        } else {
            // SAFETY: this slot uniquely owns its lower, acyclic subtree.
            unsafe { destroy_tree::<P>(entry.cast()) };
        }
    }
}

unittest::path_mod_test!(tests, "../ut-tests/insert.rs");
unittest::path_mod_test!(tests_remove, "../ut-tests/remove.rs");

#[cfg(target_os = "none")]
unittest::path_mod_test!(tests_context, "../ut-tests/context.rs");
#[cfg(all(target_os = "none", feature = "smp"))]
unittest::path_mod_test!(tests_migration, "../ut-tests/migration.rs");
#[cfg(target_os = "none")]
unittest::path_mod_test!(tests_tasks, "../ut-tests/tasks.rs");

#[cfg(test)]
#[path = "../tests/host/storage.rs"]
mod tests_storage;

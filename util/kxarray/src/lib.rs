// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Sparse usize-indexed ownership with lockless, RCU-scoped borrowing.
//!
//! `XArray<KBox<T>>` owns each inserted object; `XArray<Arc<T>>` owns one strong
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
//! Default updates require reclaim-safe, sleepable task context with IRQs and
//! preemption enabled and no locks needed by reclaim, IO or filesystem entry.
//! Successful replacement/removal publishes a change; it does not mean old
//! objects have been destroyed. Stored data needs its own mutation protocol.
//!
//! ```no_run
//! extern crate alloc;
//! use kalloc_types::KBox;
//!
//! # fn example() -> Result<(), kerrno::KError> {
//! let entries = kxarray::XArray::<KBox<usize>>::new();
//! let first = KBox::new_nowait(7)?;
//! // SAFETY: initialized task context, primitive payload and the kernel's
//! // nonblocking allocator release; no callback-needed locks are held.
//! unsafe { entries.insert(42, first) }.map_err(|(error, _owner)| error)?;
//! let copied = krcu::with_read(|scope| entries.read(scope, 42).copied());
//! assert_eq!(copied, Some(7));
//! let replacement = KBox::new_nowait(9)?;
//! // SAFETY: same audited primitive release chain; eligible task outside
//! // read scopes/locks, with the permanent RCU service remaining available.
//! unsafe { entries.set(42, replacement) }.map_err(|(error, _owner)| error)?;
//! assert_eq!(
//!     krcu::with_read(|scope| entries.read(scope, 42).copied()),
//!     Some(9)
//! );
//! // SAFETY: eligible task and live RCU service, no callback-required lock.
//! let removed = unsafe { entries.remove(42) }?;
//! assert!(removed);
//! assert_eq!(
//!     krcu::with_read(|scope| entries.read(scope, 42).copied()),
//!     None
//! );
//! drop(entries); // Retired callbacks do not borrow the array.
//! //
//! # Ok(())
//! # }
//! ```
//!
//! With Arc, an explicitly acquired owner can outlive both the read scope and
//! the association. The caller is responsible for that owner's final Drop:
//!
//! ```no_run
//! extern crate alloc;
//! use alloc::sync::Arc;
//!
//! # fn example() -> Result<(), kerrno::KError> {
//! let entries = kxarray::XArray::<Arc<usize>>::new();
//! // SAFETY: allocation-permitted task; usize and Arc storage have a
//! // nonblocking release chain, including last-reference destruction.
//! unsafe { entries.insert(5, Arc::new(11)) }.map_err(|(error, _owner)| error)?;
//! let retained = krcu::with_read(|scope| entries.get_owned(scope, 5));
//! // SAFETY: initialized watched task outside scopes/locks, permanent RCU service.
//! let removed = unsafe { entries.remove(5) }?;
//! assert!(removed);
//! drop(entries);
//! assert_eq!(retained.as_deref(), Some(&11));
//! drop(retained); // This need not be the last Arc: retirement may still be pending.
//! //
//! # Ok(())
//! # }
//! ```
//!
//! Before RCU service readiness, new insertions and root growth are permitted
//! with explicit NOWAIT permissions once allocation and local IRQ/per-CPU
//! prerequisites exist. Default updates require sleepable, reclaim-safe task
//! context. Replacement and
//! removal need runtime admission. There is no per-array callback drain; use
//! the system RCU drain contract separately when shutdown needs completion.
#![no_std]

extern crate alloc;

use alloc::sync::Arc;
use core::{
    marker::PhantomData,
    sync::atomic::{AtomicPtr, Ordering},
};

use kalloc_types::{AllocFlags, KBox};
use kerrno::{KError, KResult};
use krcu::ReadScope;
use kspin::SpinNoPreempt;

mod node;
mod owner;
mod prepared;
mod retirement;

use node::{MAX_HEIGHT, Node, PreparedNodes, height_for};
pub use owner::EntryOwner;
pub use prepared::{PendingRetirement, PreparedInsert, PreparedRemove};
use retirement::Retirement;

trait Allocator {
    fn allocate<T>(&mut self, value: T) -> KResult<KBox<T>>;
}

impl Allocator for AllocFlags {
    fn allocate<T>(&mut self, value: T) -> KResult<KBox<T>> {
        KBox::new_with_flags(value, *self)
    }
}

fn validate_flags(flags: AllocFlags) -> KResult {
    if flags.bits() & !AllocFlags::all().bits() != 0 {
        return Err(KError::InvalidInput);
    }
    Ok(())
}

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
/// use kalloc_types::KBox;
/// fn inject<'short>(array: &kxarray::XArray<KBox<&'static usize>>, value: &'short usize)
///     -> Result<(), kerrno::KError>
/// {
///     let narrowed: &kxarray::XArray<KBox<&'short usize>> = array;
///     let owned = KBox::new_nowait(value)?;
///     // SAFETY: primitive reference payload has no blocking destructor.
///     unsafe { narrowed.insert(0, owned) }.map_err(|(error, _owner)| error)?;
///     Ok(())
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
    /// use kalloc_types::KBox;
    /// let array = kxarray::XArray::<KBox<usize>>::new();
    /// let escaped = krcu::with_read(|scope| array.read(scope, 0));
    /// println!("{escaped:?}");
    /// ```
    ///
    /// The array must also remain alive for the entire borrow:
    ///
    /// ```compile_fail,E0505
    /// use kalloc_types::KBox;
    /// let array = kxarray::XArray::<KBox<usize>>::new();
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
    /// KERNEL allocation requires sleepable, reclaim-safe task context, enabled IRQs
    /// and preemption, and no locks needed by reclaim/IO/filesystem entry. Use
    /// explicit NOWAIT permissions for allocation-permitted early boot. Initial
    /// insertion and root growth do not require a running reclamation service.
    ///
    /// # Errors
    /// Returns `(AlreadyExists, value)` on conflict or `(NoMemory, value)` on
    /// reservation failure, preserving the offered owner. A failed operation
    /// publishes nothing; concurrent writers may independently change the tree.
    ///
    /// # Safety
    /// Call from sleepable, reclaim-safe task context with local IRQ/per-CPU
    /// primitives initialized; never from IRQ, softirq or NMI or an RCU scope.
    /// The offered owner's complete release chain must not block or unwind and
    /// must be valid in ordinary RCU callback context, including nested values,
    /// last strong-reference destruction and allocator deallocation. This is an
    /// adoption contract, not a consequence of Send/Sync. Rejected ownership
    /// remains the caller's responsibility.
    pub unsafe fn insert(&self, index: usize, value: P) -> Result<(), (KError, P)> {
        // SAFETY: insert's unsafe contract requires initialized local IRQ/per-CPU
        // primitives and sleepable reclaim-safe task context, outside RCU scopes
        // and reclaim/IO/filesystem locks. It also requires the offered owner's
        // nested drops, final Arc release and deallocation to be nonblocking and
        // non-unwinding in RCU callbacks. Forwarding the same owner with KERNEL
        // permissions preserves these caller-established obligations.
        unsafe { self.insert_with_flags(index, value, AllocFlags::KERNEL) }
    }

    /// Prepares private capacity for one insertion, without inspecting the array.
    ///
    /// Does not reserve the key or publish anything. Capacity covers any later
    /// root shape, at the cost of up to twice the maximum tree height in nodes.
    /// Invalid flags or allocation failure return an error without mutation.
    /// Cancel by dropping the result; no RCU service is needed.
    ///
    /// # Safety
    /// Use initialized local primitives and the allocation context required by
    /// [`Self::insert_with_flags`]. NOWAIT permits allocation-ready early boot,
    /// not interrupt/NMI context. Drop unused preparation only where allocator
    /// deallocation is permitted.
    pub unsafe fn prepare_insert(
        &self,
        index: usize,
        mut flags: AllocFlags,
    ) -> KResult<PreparedInsert> {
        validate_flags(flags)?;
        PreparedInsert::with_allocator(index, &mut flags)
    }

    /// Inserts at the prepared key without allocating or waiting for RCU.
    ///
    /// Consumes preparation on both success and conflict. An occupied key returns
    /// `(AlreadyExists, value)` without changing the association. Spare private
    /// nodes are deallocated after unlocking; this is not a no-deallocation API.
    /// Writers must separately serialize multi-array transactions; preparation
    /// does not prevent a competing insertion.
    ///
    /// # Safety
    /// Uphold [`Self::insert`]'s nonblocking, non-unwinding payload release and
    /// initialized task/early-boot context requirements. This operation requires
    /// no sleeping allocation permission, but cannot run in IRQ/NMI context or
    /// a read scope. Local guard release may run a scheduling checkpoint.
    pub unsafe fn insert_prepared(
        &self,
        value: P,
        mut prepared: PreparedInsert,
    ) -> Result<(), (KError, P)> {
        let guard = self.writers.lock();
        if self.nodes_needed(prepared.index).is_none() {
            drop(guard);
            return Err((KError::AlreadyExists, value));
        }
        self.publish(prepared.index, value, &mut prepared.nodes);
        drop(guard);
        Ok(())
    }

    /// Performs [`Self::insert`] with explicit allocation permissions.
    ///
    /// Flags govern only private node/retirement storage, never payload construction
    /// or destruction. Invalid bits return InvalidInput before inspecting the tree.
    /// NOWAIT does not grant IRQ/NMI access, reserves or bounded latency. NOIO/NOFS
    /// still permit sleeping; callers must exclude locks needed by allowed reclaim.
    ///
    /// # Safety
    /// Uphold [`Self::insert`]'s release-chain and RCU obligations. Without sleeping
    /// reclaim, insertion/root growth may also run in initialized early boot;
    /// replacement/removal still require eligible watched task context outside RCU
    /// scopes. KERNEL requires sleepable, reclaim-safe task context, IRQs and
    /// preemption enabled, and no lock needed by reclaim, IO or filesystem entry.
    pub unsafe fn insert_with_flags(
        &self,
        index: usize,
        value: P,
        mut flags: AllocFlags,
    ) -> Result<(), (KError, P)> {
        if let Err(error) = validate_flags(flags) {
            return Err((error, value));
        }
        // SAFETY: the unsafe entry contract establishes initialized IRQ/per-CPU
        // primitives, no RCU read scope, and a nonblocking, non-unwinding owner
        // release chain. The caller supplies either reclaim-safe task context
        // for sleeping permissions or allocation-permitted early boot for NOWAIT.
        // validate_flags rejected unknown bits; the AllocFlags allocator forwards
        // these same permissions to KBox, and preparation releases the writer lock.
        unsafe { self.insert_with_allocator(index, value, &mut flags) }
    }

    /// # Safety
    /// The allocator must obey the caller-selected permissions; all other
    /// obligations match the corresponding public update operation.
    unsafe fn insert_with_allocator(
        &self,
        index: usize,
        value: P,
        allocator: &mut impl Allocator,
    ) -> Result<(), (KError, P)> {
        let mut prepared = PreparedNodes::new();
        loop {
            let guard = self.writers.lock();
            let Some(needed) = self.nodes_needed(index) else {
                drop(guard);
                return Err((KError::AlreadyExists, value));
            };
            if prepared.len < needed {
                drop(guard);
                if let Err(error) = prepared.prepare(needed, allocator) {
                    return Err((error, value));
                }
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
    /// Node/retirement reservation failure returns `(NoMemory, value)` before
    /// publication. Default KERNEL permissions require the context of insert.
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
        // SAFETY: set's unsafe contract establishes initialized IRQ/per-CPU state,
        // sleepable reclaim-safe context outside RCU scopes and scheduler/RCU
        // locks, and a nonblocking, non-unwinding complete payload release chain.
        // The caller retains permanent RCU service/CPU availability; the delegated
        // replacement path checks runtime admission before publishing. KERNEL
        // permissions match this caller-established context.
        unsafe { self.set_with_flags(index, value, AllocFlags::KERNEL) }
    }

    /// Performs [`Self::set`] with explicit allocation permissions.
    ///
    /// Flags govern only private node/retirement storage, never payload construction
    /// or destruction. Invalid bits return InvalidInput before inspecting the tree.
    /// NOWAIT does not grant IRQ/NMI access, reserves or bounded latency. NOIO/NOFS
    /// still permit sleeping; callers must exclude locks needed by allowed reclaim.
    ///
    /// # Safety
    /// Uphold [`Self::set`]'s release-chain and RCU obligations. Without sleeping
    /// reclaim, insertion/root growth may also run in initialized early boot;
    /// replacement/removal still require eligible watched task context outside RCU
    /// scopes. KERNEL requires sleepable, reclaim-safe task context, IRQs and
    /// preemption enabled, and no lock needed by reclaim, IO or filesystem entry.
    pub unsafe fn set_with_flags(
        &self,
        index: usize,
        value: P,
        mut flags: AllocFlags,
    ) -> Result<(), (KError, P)>
    where
        P: 'static,
    {
        if let Err(error) = validate_flags(flags) {
            return Err((error, value));
        }
        // SAFETY: the caller establishes the same initialized execution context and
        // nonblocking, non-unwinding full owner release chain required by set,
        // excludes scheduler/RCU and reclaim-needed locks, and keeps the permanent
        // service/CPUs available for retirement. Validated flags are forwarded
        // unchanged by the allocator. The helper preflights occupied replacement
        // and prepares outside the writer lock before rechecking publication.
        unsafe { self.set_with_allocator(index, value, &mut flags) }
    }

    /// # Safety
    /// The allocator must obey the caller-selected permissions; all other
    /// obligations match the corresponding public update operation.
    unsafe fn set_with_allocator(
        &self,
        index: usize,
        value: P,
        allocator: &mut impl Allocator,
    ) -> Result<(), (KError, P)>
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
                    if let Err(error) = prepared.prepare(needed, allocator) {
                        return Err((error, value));
                    }
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
            retirement = Some(match Retirement::<P>::prepare(allocator) {
                Ok(record) => record,
                Err(error) => return Err((error, value)),
            });
            #[cfg(unittest)]
            tests::after_set_preparation(self, index);
            // Reinspect after allocation: never use a path retained across unlock.
        }
    }

    /// Allocates private retirement storage without inspecting or withdrawing an entry.
    ///
    /// Invalid flags or allocation failure leave the array unchanged. The result
    /// is storage only, not a transferable RCU readiness/context permit. It may
    /// be cancelled by dropping it before RCU service startup.
    ///
    /// # Safety
    /// Supply the allocation context required by [`Self::prepare_insert`], and
    /// cancel only where allocator deallocation is permitted. No payload release
    /// or RCU submission occurs in this operation.
    pub unsafe fn prepare_remove(&self, mut flags: AllocFlags) -> KResult<PreparedRemove<P>> {
        validate_flags(flags)?;
        PreparedRemove::with_allocator(&mut flags)
    }

    /// Removes the entry currently at `index` using preallocated retirement storage.
    ///
    /// Rechecks RCU admission before any mutation, including for an absent key.
    /// Returns a preflight error unchanged, false for absence, or true after
    /// withdrawal and callback submission. No allocation occurs. Unused storage
    /// is deallocated outside the writer lock. Preparation is consumed on every
    /// outcome and does not identify an earlier version of this entry: callers
    /// requiring identity-matched removal must serialize all competing writers.
    ///
    /// # Safety
    /// Uphold [`Self::remove`]'s context, stored-owner release and permanent-service
    /// obligations. Hold no RCU scope or scheduler/bookkeeping lock. Preparation
    /// does not relax these conditions or make two arrays an atomic transaction.
    pub unsafe fn remove_prepared(&self, index: usize, prepared: PreparedRemove<P>) -> KResult<bool>
    where
        P: 'static,
    {
        krcu::preflight_callback_submission()?;
        // SAFETY: current preflight precedes the internal writer critical
        // section; this method's caller supplies release and service obligations.
        let Some(retirement) = (unsafe { self.withdraw_prepared(index, prepared) }) else {
            return Ok(false);
        };
        // SAFETY: withdrawal released the writer guard; no caller code or
        // explicit context transition intervenes before submission.
        unsafe { retirement.retire() };
        Ok(true)
    }

    /// Withdraws an entry without allocating or submitting its RCU retirement.
    ///
    /// This advanced entrance separates index mutation from callback submission
    /// for callers serializing several indexes under their own short lock.
    /// It consumes preparation and returns None for an absent key, or ownership
    /// of the pending retirement. No payload is returned and no grace period
    /// has yet begun on behalf of this removal. The caller must retire every
    /// returned record after releasing its locks, even if later work fails.
    /// Preparation and this operation do not reserve a key or compare identities.
    ///
    /// # Safety
    /// Successfully call [`krcu::preflight_callback_submission`] in the current
    /// task before acquiring caller-held writer exclusion. Uphold insertion's
    /// payload-release obligations and the permanent-service contract. Between
    /// preflight, withdrawal and [`PendingRetirement::retire`], allow only the
    /// index transaction and temporary exclusion guards, not arbitrary caller
    /// code, explicit context changes or RCU scopes. IRQ masking/preemption
    /// exclusion for the transaction is allowed; IRQ/NMI/BH callers are not.
    /// Release those guards and restore preflight context before retirement.
    /// Serialize competing writers when removing a previously selected identity.
    /// This operation deliberately does not recheck sleepable context under
    /// the caller's short lock; use [`Self::remove_prepared`] otherwise.
    pub unsafe fn withdraw_prepared(
        &self,
        index: usize,
        prepared: PreparedRemove<P>,
    ) -> Option<PendingRetirement<P>>
    where
        P: 'static,
    {
        let guard = self.writers.lock();
        // SAFETY: writer exclusion retains the current path and payload owner.
        if unsafe { self.lookup(index) }.is_null() {
            drop(guard);
            return None;
        }
        // SAFETY: the occupied path was checked under this same writer guard.
        let (old, detached) = unsafe { self.withdraw(index) };
        drop(guard);
        Some(PendingRetirement {
            record: prepared.record,
            owner: old,
            detached,
        })
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
    /// removal requires successful preflight. Retirement reservation failure returns
    /// NoMemory before withdrawal. KERNEL requires sleepable, reclaim-safe
    /// task context without locks needed by reclaim/IO/filesystem entry.
    ///
    /// # Safety
    /// Uphold [`Self::set`]'s execution-context and permanent-service obligations.
    /// Stored owners have already been admitted under the insertion release
    /// contract. No scheduler/RCU bookkeeping lock may be held.
    pub unsafe fn remove(&self, index: usize) -> Result<bool, KError>
    where
        P: 'static,
    {
        // SAFETY: remove's unsafe contract requires sleepable reclaim-safe task
        // context with initialized IRQ/per-CPU state, no read scope or scheduler/
        // RCU/reclaim-needed locks, and permanent service/CPU availability through
        // callback execution. Stored owners were admitted with nonblocking,
        // non-unwinding release chains by insertion. Delegation preserves KERNEL
        // permissions and performs runtime preflight before withdrawal.
        unsafe { self.remove_with_flags(index, AllocFlags::KERNEL) }
    }

    /// Performs [`Self::remove`] with explicit allocation permissions.
    ///
    /// Flags govern only private node/retirement storage, never payload construction
    /// or destruction. Invalid bits return InvalidInput before inspecting the tree.
    /// NOWAIT does not grant IRQ/NMI access, reserves or bounded latency. NOIO/NOFS
    /// still permit sleeping; callers must exclude locks needed by allowed reclaim.
    ///
    /// # Safety
    /// Uphold [`Self::remove`]'s release-chain and RCU obligations. Without sleeping
    /// reclaim, insertion/root growth may also run in initialized early boot;
    /// replacement/removal still require eligible watched task context outside RCU
    /// scopes. KERNEL requires sleepable, reclaim-safe task context, IRQs and
    /// preemption enabled, and no lock needed by reclaim, IO or filesystem entry.
    pub unsafe fn remove_with_flags(
        &self,
        index: usize,
        mut flags: AllocFlags,
    ) -> Result<bool, KError>
    where
        P: 'static,
    {
        validate_flags(flags)?;
        // SAFETY: the caller establishes eligible watched task context outside read
        // scopes and scheduler/RCU/reclaim-needed locks, with initialized local
        // primitives and permanent service/CPU availability. Insertion already
        // admitted the stored owners' nonblocking, non-unwinding release chains.
        // The allocator preserves validated permissions; the helper preflights
        // before withdrawal and rechecks ownership under the writer lock.
        unsafe { self.remove_with_allocator(index, &mut flags) }
    }

    /// # Safety
    /// The allocator must obey the caller-selected permissions; all other
    /// obligations match the corresponding public update operation.
    unsafe fn remove_with_allocator(
        &self,
        index: usize,
        allocator: &mut impl Allocator,
    ) -> Result<bool, KError>
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
                // Each non-null detached pointer is an empty node uniquely
                // unlinked under that same guard and covered by that preflight.
                // No explicit context transition precedes unlocked submission.
                unsafe { Retirement::<P>::submit_with_nodes(record, old, detached) };
                return Ok(true);
            }
            drop(guard);
            retirement = Some(Retirement::<P>::prepare(allocator)?);
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
            // root transfers its graph ownership without reconstructing a KBox.
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
    /// Removes a quiescent synthetic fixture without pruning shared radix nodes.
    ///
    /// This test-only entrance is not an RCU retirement test. Nodes stay in the
    /// tree until reuse or container destruction, allowing unrelated-key readers.
    ///
    /// # Safety
    /// Exclude all readers and competing writers of this key, including readers
    /// retaining a borrowed payload after lookup. Owned Arc results may survive.
    /// Call only from allocator-capable task context, never IRQ/NMI/BH.
    #[cfg(unittest)]
    pub unsafe fn take_quiescent_fixture(&self, index: usize) -> Option<Arc<T>> {
        let _guard = self.writers.lock();
        let mut raw = self.root.load(Ordering::Acquire);
        if raw.is_null() {
            return None;
        }
        // SAFETY: writer exclusion retains every traversed node.
        if !unsafe { &*raw }.covers(index) {
            return None;
        }
        while !raw.is_null() {
            // SAFETY: writer exclusion retains this initialized node.
            let node = unsafe { &*raw };
            if node.height == 1 {
                if node.slot(index).load(Ordering::Acquire).is_null() {
                    return None;
                }
                let owner = node.remove_slot(index).cast::<T>();
                // SAFETY: the slot transfers exactly one Arc owner; the caller
                // excluded borrowed readers of this fixture. No nodes detach.
                return Some(unsafe { Arc::from_raw(owner) });
            }
            raw = node.slot(index).load(Ordering::Acquire).cast();
        }
        None
    }

    /// Acquires an independent owner under writer exclusion, without an RCU scope.
    ///
    /// Intended for control paths and early boot after local lock primitives are
    /// initialized. No allocation or caller callback occurs under the lock, and
    /// the returned owner may outlive the array. The caller is responsible for
    /// its eventual destruction context.
    ///
    /// Do not call from IRQ, NMI or serving-softirq context: the interrupted
    /// code may hold this same preemption-disabling writer lock. Interrupt
    /// readers must use [`Self::get_owned`] with a real RCU scope instead.
    /// Releasing the guard may run the normal preemption checkpoint; this is
    /// not a no-schedule primitive. No initialized RCU service is required.
    pub fn get_owned_locked(&self, index: usize) -> Option<Arc<T>> {
        let _guard = self.writers.lock();
        // SAFETY: writer exclusion prevents detaching any traversed node or
        // payload, and the shared array borrow excludes container destruction.
        let raw = unsafe { self.lookup(index) }.cast::<T>();
        if raw.is_null() {
            return None;
        }
        // SAFETY: the stored strong owner cannot be withdrawn under this lock;
        // raw is the original Arc allocation pointer, not a payload reborrow.
        unsafe { Arc::increment_strong_count(raw) };
        // SAFETY: consume only the newly acquired reference. The returned owner
        // is constructed before writer exclusion ends and is dropped by its caller.
        Some(unsafe { Arc::from_raw(raw) })
    }

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
    // SAFETY: exclusive graph ownership permits reconstructing this node KBox.
    let node = unsafe { KBox::from_raw(raw) };
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

#[cfg(test)]
#[path = "../tests/host/storage.rs"]
mod tests_storage;

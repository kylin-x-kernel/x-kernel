// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Fallible shared ownership, adapted from PR #385's KArc/KWeak interface.

use alloc::sync::{Arc, Weak};
use core::{
    alloc::{AllocError, Allocator, Layout},
    fmt,
    ops::Deref,
    ptr::NonNull,
};

use kerrno::{KError, KResult};

use crate::{AllocFlags, kbox};

#[derive(Clone, Copy)]
struct SharedAllocator {
    flags: AllocFlags,
    #[cfg(any(test, unittest))]
    fail: bool,
}

impl SharedAllocator {
    /// All instances use the same build-time allocation domain. Flags and the
    /// test failure switch affect allocation only, never release. Raw-owner
    /// recovery depends on this equivalence; stateful backends must preserve
    /// their release authority instead of using this reconstructed allocator.
    const fn for_release() -> Self {
        Self {
            flags: AllocFlags::NOWAIT,
            #[cfg(any(test, unittest))]
            fail: false,
        }
    }
}

/// SAFETY: Allocation uses the same layout-based backend as KBox. Arc supplies the
/// original live allocation and layout for deallocation exactly once, after
/// the last weak reference. Copies retain the same immutable backend policy.
unsafe impl Allocator for SharedAllocator {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        #[cfg(any(test, unittest))]
        if self.fail {
            return Err(AllocError);
        }
        if layout.size() == 0 {
            return Ok(NonNull::slice_from_raw_parts(layout.dangling_ptr(), 0));
        }
        kbox::allocate(layout, self.flags)
            .map(|pointer| NonNull::slice_from_raw_parts(pointer, layout.size()))
            .map_err(|_| AllocError)
    }

    /// # Safety
    /// The Allocator caller transfers a live allocation from this backend,
    /// with its original layout and no remaining accesses to the allocation.
    unsafe fn deallocate(&self, pointer: NonNull<u8>, layout: Layout) {
        if layout.size() == 0 {
            return;
        }
        #[cfg(target_os = "none")]
        kalloc::global_allocator().dealloc(pointer, layout);
        #[cfg(not(target_os = "none"))]
        // SAFETY: Arc transfers this live host allocation with its original layout.
        unsafe {
            alloc::alloc::dealloc(pointer.as_ptr(), layout);
        }
    }
}

/// Fallibly allocated shared ownership of a sized value.
///
/// Clone and downgrade do not allocate. The last strong owner runs `T::drop`
/// in its own context; the last weak owner releases storage. Allocation flags
/// do not constrain either destructor. Reference-count and storage cleanup do
/// not enter reclaim; arbitrary T::drop may allocate, sleep or perform IO.
/// Callers must respect payload and allocator lock/reentrancy requirements.
/// No NMI safety or bounded latency is promised. Strong cycles leak unless
/// broken by the owner; use KWeak for back-references.
///
/// The private standard Arc implementation supplies reference counting,
/// overflow protection and unwind cleanup. This type does not expose a
/// conversion to standard Arc, cyclic construction or DSTs. Raw ownership
/// transfers exactly one strong reference through into_raw/from_raw.
///
/// Shared references cannot move a non-Send payload to another thread:
/// ```compile_fail
/// use kalloc_types::KArc;
/// use std::sync::MutexGuard;
/// fn require_sync<T: Sync>() {}
/// require_sync::<MutexGuard<'static, ()>>();
/// require_sync::<KArc<MutexGuard<'static, ()>>>();
/// ```
pub struct KArc<T>(Arc<T, SharedAllocator>);

/// A non-owning reference to a KArc allocation.
///
/// Weak references retain storage, not the value. Upgrade neither allocates
/// nor resurrects a destroyed value. Final drop obeys the allocator's
/// deallocation context requirements, independently of the original flags.
///
/// Upgrade must not allow a non-Send payload to move across threads:
/// ```compile_fail
/// use kalloc_types::KWeak;
/// use std::sync::MutexGuard;
/// fn require_sync<T: Sync>() {}
/// require_sync::<MutexGuard<'static, ()>>();
/// require_sync::<KWeak<MutexGuard<'static, ()>>>();
/// ```
pub struct KWeak<T>(Weak<T, SharedAllocator>);

impl<T> KArc<T> {
    /// Allocates with KERNEL permissions in a sleepable, reclaim-safe task.
    ///
    /// IRQs/preemption must permit sleeping; do not hold locks required by
    /// reclaim, IO or filesystem callbacks, or a non-sleepable RCU read guard.
    /// Returns NoMemory on allocation failure and drops the supplied value.
    /// Nested value construction is outside this allocation contract.
    pub fn new(value: T) -> KResult<Self> {
        Self::new_with_flags(value, AllocFlags::KERNEL)
    }

    /// Allocates without granting sleeping reclaim, IO or filesystem entry.
    ///
    /// Returns NoMemory on failure and drops the supplied value in the caller's
    /// context. NOWAIT does not promise reserves, NMI safety or bounded latency.
    pub fn new_nowait(value: T) -> KResult<Self> {
        Self::new_with_flags(value, AllocFlags::NOWAIT)
    }

    /// Allocates with explicit permissions, using the KBox backend directly.
    ///
    /// DIRECT_RECLAIM requires the same context as new. Unknown flag bits
    /// return InvalidInput, including for zero-sized values; allocation failure
    /// returns NoMemory. Both errors drop the supplied value without publishing
    /// an owner. Even a zero-sized value needs a reference-count allocation.
    pub fn new_with_flags(value: T, flags: AllocFlags) -> KResult<Self> {
        Self::new_in(
            value,
            SharedAllocator {
                flags,
                #[cfg(any(test, unittest))]
                fail: false,
            },
        )
    }

    fn new_in(value: T, allocator: SharedAllocator) -> KResult<Self> {
        if allocator.flags.bits() & !AllocFlags::all().bits() != 0 {
            return Err(KError::InvalidInput);
        }
        Arc::try_new_in(value, allocator)
            .map(Self)
            .map_err(|_| KError::NoMemory)
    }

    /// Borrows the payload address without transferring or adding ownership.
    ///
    /// No allocation or sleeping occurs. The caller must retain a strong
    /// owner for every dereference. This pointer alone cannot be passed to
    /// from_raw: it does not represent a transferred strong reference.
    pub fn as_ptr(this: &Self) -> *const T {
        Arc::as_ptr(&this.0)
    }

    /// Transfers one strong reference to a raw pointer without allocation,
    /// sleeping, destruction or changing its reference count.
    ///
    /// Recover that reference exactly once with from_raw to avoid a leak.
    /// Copying the pointer does not clone ownership. This is not a standard
    /// Arc or KBox pointer conversion.
    #[must_use]
    pub fn into_raw(this: Self) -> *const T {
        Arc::into_raw_with_allocator(this.0).0
    }

    /// Recovers one strong reference previously transferred by into_raw.
    ///
    /// Does not allocate or sleep. Later destruction retains KArc's ordinary
    /// payload/deallocation context requirements, not the original flags.
    ///
    /// # Safety
    /// The pointer must originate from KArc::<T>::into_raw with the same T,
    /// address and provenance, and represent one still-unclaimed strong
    /// reference. Recover it exactly once per transfer. An as_ptr borrow,
    /// Weak pointer, standard Arc pointer or KBox pointer is not sufficient.
    /// Raw transfer does not relax T's thread-safety or borrowing requirements.
    pub unsafe fn from_raw(pointer: *const T) -> Self {
        // SAFETY: The caller transfers one unrecovered KArc reference of T.
        // Every SharedAllocator uses the same backend and layout-based release;
        // allocation flags carry no release authority or per-allocation state.
        Self(unsafe { Arc::from_raw_in(pointer, SharedAllocator::for_release()) })
    }

    /// Creates a weak reference without allocation or sleeping.
    pub fn downgrade(this: &Self) -> KWeak<T> {
        KWeak(Arc::downgrade(&this.0))
    }

    /// Tests allocation identity, not value equality, without allocation.
    pub fn ptr_eq(this: &Self, other: &Self) -> bool {
        Arc::ptr_eq(&this.0, &other.0)
    }

    /// Returns a strong-count snapshot, not a uniqueness synchronization guard.
    pub fn strong_count(this: &Self) -> usize {
        Arc::strong_count(&this.0)
    }

    /// Returns a weak-count snapshot, excluding the implicit weak reference.
    pub fn weak_count(this: &Self) -> usize {
        Arc::weak_count(&this.0)
    }

    /// Moves out the value if this is the sole strong owner, otherwise returns
    /// the unchanged owner. Existing weak references do not prevent extraction.
    /// Does not allocate or destroy the extracted value. Storage release, when
    /// no weak references remain, retains the usual deallocation constraints.
    pub fn try_unwrap(this: Self) -> Result<T, Self> {
        Arc::try_unwrap(this.0).map_err(Self)
    }
}

impl<T> Clone for KArc<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> Deref for KArc<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> AsRef<T> for KArc<T> {
    fn as_ref(&self) -> &T {
        self
    }
}

impl<T: fmt::Debug> fmt::Debug for KArc<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, formatter)
    }
}

impl<T> KWeak<T> {
    /// Constructs an empty weak handle without allocating.
    pub fn new() -> Self {
        Self(Weak::new_in(SharedAllocator::for_release()))
    }

    /// Acquires a strong owner if the value is still alive, without allocating.
    pub fn upgrade(&self) -> Option<KArc<T>> {
        self.0.upgrade().map(KArc)
    }

    /// Tests allocation identity. Two empty handles compare equal.
    pub fn ptr_eq(this: &Self, other: &Self) -> bool {
        Weak::ptr_eq(&this.0, &other.0)
    }
}

impl<T> Default for KWeak<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Clone for KWeak<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> fmt::Debug for KWeak<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("KWeak")
    }
}

unittest::path_mod_test!(mod tests, "../ut-tests/karc.rs");

#[cfg(all(test, not(unittest)))]
#[path = "../tests/host/karc.rs"]
mod host_tests;

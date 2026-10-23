// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{
    alloc::Layout,
    marker::PhantomData,
    ops::{Deref, DerefMut},
    ptr::NonNull,
};

use kerrno::{KError, KResult};

use crate::AllocFlags;

fn allocate(layout: Layout, _flags: AllocFlags) -> KResult<NonNull<u8>> {
    kalloc::global_allocator()
        .alloc(layout)
        .map_err(|_| KError::NoMemory)
}

struct Storage<T> {
    pointer: NonNull<T>,
    owner: PhantomData<T>,
}

impl<T> Drop for Storage<T> {
    fn drop(&mut self) {
        let layout = Layout::new::<T>();
        if layout.size() != 0 {
            kalloc::global_allocator().dealloc(self.pointer.cast(), layout);
        }
    }
}

/// An exclusively owned, fallibly allocated heap value.
///
/// Moving the owner preserves the value's address; mutable access can still
/// replace the value. This type does not provide pinning or implicit cloning.
/// Dropping the owner destroys the value and releases its storage. A zero-sized
/// value needs no allocation, but its destructor still runs.
///
/// # Execution context
/// Borrowing through `Deref`/`DerefMut` neither allocates nor sleeps; operations
/// on `T` retain their own context requirements. Dropping the owner does not
/// perform reclaim, but executes `T::drop` in the dropping caller's context.
/// Allocation flags are not retained and do not constrain that destructor.
pub struct KBox<T> {
    storage: Storage<T>,
}

impl<T> KBox<T> {
    /// Allocates using the default [`AllocFlags::KERNEL`] policy.
    ///
    /// # Execution context
    /// The caller must permit sleeping, including future synchronous reclaim,
    /// IO and filesystem callbacks. Requires sleepable task context, with no
    /// non-sleeping lock, IRQ/preemption-disabled region, interrupt handler,
    /// or non-sleepable RCU read-side critical section enclosing the call.
    /// Even a sleepable lock must not be needed by the permitted reclaim path.
    /// These requirements hold today, including for zero-sized values; the
    /// current backend's lack of reclaim is not an exemption. This method does
    /// not detect the current context or downgrade automatically to NOWAIT.
    ///
    /// # Errors
    /// Returns [`KError::NoMemory`] if storage cannot be allocated, dropping
    /// `value`. See [`Self::new_with_flags`] for the construction boundary.
    pub fn new(value: T) -> KResult<Self> {
        Self::new_with_flags(value, AllocFlags::default())
    }

    /// Allocates without sleeping, using [`AllocFlags::NOWAIT`].
    ///
    /// # Execution context
    /// The storage allocation does not sleep or enter reclaim IO/filesystem
    /// callbacks. This is not a blanket guarantee that the whole operation is
    /// interrupt-safe: the caller must also satisfy the allocator's reentrancy
    /// and lock-order requirements. NMI safety is not promised.
    /// No reserve access or bounded latency is promised. The caller remains
    /// responsible for any allocation or blocking in value construction/drop,
    /// especially the input destructor executed here on allocation failure.
    ///
    /// # Errors
    /// Returns [`KError::NoMemory`] if storage cannot be allocated, dropping
    /// `value`.
    pub fn new_nowait(value: T) -> KResult<Self> {
        Self::new_with_flags(value, AllocFlags::NOWAIT)
    }

    /// Allocates storage with caller-selected permissions and moves `value` into it.
    ///
    /// # Execution context
    /// With [`AllocFlags::DIRECT_RECLAIM`], all sleepability constraints of
    /// [`Self::new`] apply. `NOIO` and `NOFS` still permit sleeping; they only
    /// restrict reclaim's IO/filesystem entry. Held locks must be compatible
    /// with the remaining permitted reclaim paths. Without `DIRECT_RECLAIM`,
    /// the non-sleeping constraints of [`Self::new_nowait`] apply.
    /// Flags do not detect context or propagate to nested allocations.
    ///
    /// Construction of `value` happens before this call; allocations inside
    /// that construction are outside this method's failure/policy contract.
    /// Failure executes `T::drop`, which must be valid in the caller's context.
    ///
    /// # Errors
    /// Returns [`KError::NoMemory`] if storage cannot be allocated, or
    /// [`KError::InvalidInput`] for unknown flag bits (including for ZSTs).
    /// The supplied value is dropped on failure; no partial owner is returned.
    pub fn new_with_flags(value: T, flags: AllocFlags) -> KResult<Self> {
        Self::new_with_allocator(value, flags, allocate)
    }

    fn new_with_allocator(
        value: T,
        flags: AllocFlags,
        allocate: impl FnOnce(Layout, AllocFlags) -> KResult<NonNull<u8>>,
    ) -> KResult<Self> {
        if flags.bits() & !AllocFlags::all().bits() != 0 {
            return Err(KError::InvalidInput);
        }
        let layout = Layout::new::<T>();
        let pointer = if layout.size() == 0 {
            NonNull::dangling()
        } else {
            allocate(layout, flags)?.cast::<T>()
        };
        let storage = Storage {
            pointer,
            owner: PhantomData,
        };
        // SAFETY: the private allocator supplies unique storage for Layout<T>,
        // or an aligned dangling pointer for a ZST. Writing initializes the slot.
        unsafe { storage.pointer.as_ptr().write(value) };
        Ok(Self { storage })
    }
}

impl<T> Deref for KBox<T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: construction initialized this exclusively owned slot; the
        // returned shared reference is bounded by the owner's shared borrow.
        unsafe { self.storage.pointer.as_ref() }
    }
}

impl<T> DerefMut for KBox<T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: construction initialized the slot and the exclusive owner
        // borrow excludes any other references for the returned lifetime.
        unsafe { self.storage.pointer.as_mut() }
    }
}

impl<T> Drop for KBox<T> {
    fn drop(&mut self) {
        // SAFETY: the slot contains one initialized T, uniquely owned here.
        // Storage remains live until field drop, including if T::drop unwinds.
        unsafe { self.storage.pointer.as_ptr().drop_in_place() };
    }
}

// SAFETY: ownership can cross threads only when the owned T can; the allocator
// supports freeing on a different CPU and no references outlive the owner.
unsafe impl<T: Send> Send for KBox<T> {}
// SAFETY: shared access exposes only &T, and mutation requires &mut KBox<T>.
unsafe impl<T: Sync> Sync for KBox<T> {}

unittest::path_mod_test!(mod tests, "../ut-tests/kbox.rs");

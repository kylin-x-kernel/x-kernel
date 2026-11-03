// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{
    alloc::Layout,
    fmt,
    marker::PhantomData,
    mem::{ManuallyDrop, MaybeUninit},
    ops::{Deref, DerefMut},
    ptr::NonNull,
};

use kerrno::{KError, KResult};

use crate::AllocFlags;

#[cfg(target_os = "none")]
fn allocate(layout: Layout, _flags: AllocFlags) -> KResult<NonNull<u8>> {
    kalloc::global_allocator()
        .alloc(layout)
        .map_err(|_| KError::NoMemory)
}

#[cfg(not(target_os = "none"))]
fn allocate(layout: Layout, _flags: AllocFlags) -> KResult<NonNull<u8>> {
    // SAFETY: callers supply a nonzero layout; null is returned as NoMemory.
    NonNull::new(unsafe { alloc::alloc::alloc(layout) }).ok_or(KError::NoMemory)
}

struct Storage {
    pointer: NonNull<u8>,
    layout: Layout,
}

impl Drop for Storage {
    fn drop(&mut self) {
        let layout = self.layout;
        if layout.size() != 0 {
            #[cfg(target_os = "none")]
            kalloc::global_allocator().dealloc(self.pointer.cast(), layout);
            #[cfg(not(target_os = "none"))]
            // SAFETY: Storage retains the unique host allocation and its original
            // nonzero layout, and runs only after pointee access has ended.
            unsafe {
                alloc::alloc::dealloc(self.pointer.as_ptr(), layout);
            }
        }
    }
}

/// An exclusively owned, fallibly allocated heap value.
///
/// Moving the owner preserves the value's address; mutable access can still
/// replace the value. This type does not provide pinning or implicit cloning.
/// Dropping the owner destroys the value and releases its storage. A zero-sized
/// value needs no allocation, but its destructor still runs.
/// Sized owners can coerce to trait objects or slices without allocating.
/// Runtime-length slices support uninitialized storage and copied values.
/// String construction is not provided. Raw ownership can be transferred with
/// [`Self::into_raw`] and recovered with [`Self::from_raw`].
///
/// # Execution context
/// Borrowing through `Deref`/`DerefMut` neither allocates nor sleeps; operations
/// on `T` retain their own context requirements. Dropping the owner does not
/// perform reclaim, but executes `T::drop` in the dropping caller's context.
/// Allocation flags are not retained and do not constrain that destructor.
#[repr(transparent)]
#[derive(core::marker::CoercePointee)]
pub struct KBox<T: ?Sized> {
    pointer: NonNull<T>,
    owner: PhantomData<T>,
}

impl<T: ?Sized> KBox<T> {
    /// Transfers ownership to a raw pointer without allocating or dropping `T`.
    ///
    /// Preserves the address and DST metadata. The caller becomes responsible
    /// for eventual destruction, normally through [`Self::from_raw`]. Merely
    /// transferring the pointer neither sleeps nor accesses the pointee.
    pub fn into_raw(owned: Self) -> *mut T {
        ManuallyDrop::new(owned).pointer.as_ptr()
    }

    /// Recovers an owner previously transferred by [`Self::into_raw`].
    ///
    /// This operation does not allocate, sleep or access the pointee. Later
    /// access and destruction retain `T`'s context requirements; the original
    /// allocation flags do not constrain the destructor.
    ///
    /// # Safety
    /// `pointer` must be the non-null, aligned pointer from `KBox::into_raw`,
    /// with its original allocation provenance, type and DST metadata intact.
    /// The value must remain initialized and its storage must not have been
    /// freed. For ZSTs, the original aligned pointer is still required.
    /// The caller must transfer the sole ownership right exactly once and
    /// ensure no outstanding references conflict with this exclusive owner.
    /// In particular, RCU readers must finish before recovering a retired owner.
    /// Pointers from Box, foreign allocators or interior fields are not accepted.
    pub unsafe fn from_raw(pointer: *mut T) -> Self {
        Self {
            // SAFETY: the caller supplies the non-null pointer of a live KBox.
            pointer: unsafe { NonNull::new_unchecked(pointer) },
            owner: PhantomData,
        }
    }
}

impl<T> KBox<T> {
    /// Reserves storage before constructing a value, using `KERNEL` permissions.
    ///
    /// The sleepable, reclaim-safe execution-context requirements of [`Self::new`]
    /// apply. Dropping the returned owner releases storage without dropping `T`.
    /// Use [`KBox::write`] to initialize it without another allocation.
    /// Returns [`KError::NoMemory`] if storage cannot be allocated.
    pub fn new_uninit() -> KResult<KBox<MaybeUninit<T>>> {
        Self::new_uninit_with_flags(AllocFlags::KERNEL)
    }

    /// Reserves uninitialized storage with caller-selected allocation permissions.
    ///
    /// The execution-context and error contracts of [`Self::new_with_flags`]
    /// apply, including validation for zero-sized values. These permissions
    /// constrain only reservation, not subsequent initialization or destruction.
    pub fn new_uninit_with_flags(flags: AllocFlags) -> KResult<KBox<MaybeUninit<T>>> {
        KBox::new_with_flags(MaybeUninit::uninit(), flags)
    }

    /// Moves a sized value out of its owner and releases the backing storage.
    ///
    /// Does not allocate, clone or run `T::drop`. The returned value owns its
    /// resources and may move to a different address; this is not a pinning API.
    /// Unsized values must remain behind an owner and cannot use this operation.
    ///
    /// # Execution context
    /// Storage release does not enter reclaim or sleep. The caller must satisfy
    /// the allocator's deallocation lock-order and reentrancy requirements;
    /// NMI safety is not promised. Using or dropping the returned value retains
    /// `T`'s own context requirements, independent of the allocation flags.
    pub fn into_inner(owned: Self) -> T {
        let owned = ManuallyDrop::new(owned);
        let storage = Storage {
            pointer: owned.pointer.cast(),
            layout: Layout::new::<T>(),
        };
        // SAFETY: consuming the unique owner excludes live borrows. Its slot
        // contains an aligned, initialized T, including aligned dangling ZSTs.
        // ManuallyDrop prevents a second drop; Storage only frees the original
        // allocation after this read transfers the value to the return place.
        unsafe { storage.pointer.cast::<T>().as_ptr().read() }
    }

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
        let owned = Self {
            pointer,
            owner: PhantomData,
        };
        // SAFETY: the private allocator supplies unique storage for Layout<T>,
        // or an aligned dangling pointer for a ZST. Writing initializes the slot.
        unsafe { owned.pointer.as_ptr().write(value) };
        Ok(owned)
    }
}

impl<T> KBox<[T]> {
    /// Reserves a runtime-length slice with the sleepable, reclaim-safe
    /// `KERNEL` context required by [`KBox::new`]. No elements are initialized.
    pub fn new_uninit_slice(len: usize) -> KResult<KBox<[MaybeUninit<T>]>> {
        Self::new_uninit_slice_with_flags(len, AllocFlags::KERNEL)
    }

    /// Reserves slice storage under the context contract of
    /// [`KBox::new_with_flags`]. Invalid flags or layout overflow return
    /// `InvalidInput`; allocation failure returns `NoMemory`. Empty slices and
    /// ZST elements require no allocation, but still validate flags and layout.
    /// Layout validation does not bound ZST element counts. Callers must bound
    /// untrusted lengths and any later per-element initialization or destruction.
    pub fn new_uninit_slice_with_flags(
        len: usize,
        flags: AllocFlags,
    ) -> KResult<KBox<[MaybeUninit<T>]>> {
        Self::reserve_slice_with_allocator(len, flags, allocate)
    }

    fn reserve_slice_with_allocator(
        len: usize,
        flags: AllocFlags,
        allocate: impl FnOnce(Layout, AllocFlags) -> KResult<NonNull<u8>>,
    ) -> KResult<KBox<[MaybeUninit<T>]>> {
        if flags.bits() & !AllocFlags::all().bits() != 0 {
            return Err(KError::InvalidInput);
        }
        let layout = Layout::array::<T>(len).map_err(|_| KError::InvalidInput)?;
        let pointer = if layout.size() == 0 {
            NonNull::dangling()
        } else {
            allocate(layout, flags)?.cast::<MaybeUninit<T>>()
        };
        Ok(KBox {
            pointer: NonNull::slice_from_raw_parts(pointer, len),
            owner: PhantomData,
        })
    }
}

impl<T: Copy> KBox<[T]> {
    /// Copies a kernel slice using the sleepable, reclaim-safe `KERNEL` policy.
    /// Errors match [`Self::new_uninit_slice_with_flags`].
    pub fn from_slice(source: &[T]) -> KResult<Self> {
        Self::from_slice_with_flags(source, AllocFlags::KERNEL)
    }

    /// Copies a kernel slice with caller-selected allocation permissions.
    /// Context and errors match [`Self::new_uninit_slice_with_flags`]. The source
    /// remains unchanged on failure. Copy ZSTs require no per-element work.
    pub fn from_slice_with_flags(source: &[T], flags: AllocFlags) -> KResult<Self> {
        let mut storage = Self::new_uninit_slice_with_flags(source.len(), flags)?;
        if core::mem::size_of::<T>() != 0 {
            for (slot, value) in storage.iter_mut().zip(source) {
                slot.write(*value);
            }
        }
        // SAFETY: every non-ZST element was copied from a valid T. ZST elements
        // contain no bytes and inherit validity from the source slice. T is Copy.
        Ok(unsafe { storage.assume_init() })
    }

    /// Allocates and fills a slice using the default sleepable, reclaim-safe
    /// `KERNEL` policy. Errors match [`Self::new_uninit_slice_with_flags`].
    pub fn new_slice(value: T, len: usize) -> KResult<Self> {
        Self::new_slice_with_flags(value, len, AllocFlags::KERNEL)
    }

    /// Allocates and fills a slice with a copied value. Flags and caller-context
    /// requirements match [`KBox::new_with_flags`]; filling does not allocate.
    /// Copy ZSTs require no per-element writes or destruction, even for large lengths.
    /// Errors match [`Self::new_uninit_slice_with_flags`].
    pub fn new_slice_with_flags(value: T, len: usize, flags: AllocFlags) -> KResult<Self> {
        let mut storage = Self::new_uninit_slice_with_flags(len, flags)?;
        if core::mem::size_of::<T>() != 0 {
            for slot in storage.iter_mut() {
                slot.write(value);
            }
        }
        // SAFETY: non-ZST elements were initialized above. For a ZST, the supplied
        // valid value proves T is inhabited and there are no bytes to initialize.
        // T is Copy, so duplicating it neither transfers resources nor runs Drop.
        Ok(unsafe { storage.assume_init() })
    }
}

impl<T> KBox<[MaybeUninit<T>]> {
    /// Transfers initialized slice storage without allocating or changing its
    /// address or length. Dropping the returned owner drops every element.
    /// ZSTs with destructors still require one destructor call per element;
    /// zero allocation size does not imply bounded destruction time.
    ///
    /// # Safety
    /// Every element must contain a valid, initialized T whose ownership can be
    /// transferred to the returned slice. Partial initialization is insufficient.
    pub unsafe fn assume_init(self) -> KBox<[T]> {
        let owned = ManuallyDrop::new(self);
        KBox {
            pointer: NonNull::slice_from_raw_parts(owned.pointer.cast::<T>(), owned.len()),
            owner: PhantomData,
        }
    }
}

impl<T> KBox<MaybeUninit<T>> {
    /// Initializes reserved storage and transfers it to a typed owner.
    ///
    /// Does not allocate, sleep, or fail. Construction of `value` happens before
    /// this call and retains its own context requirements. Any previously written
    /// value in the slot is overwritten without being dropped, as with
    /// `MaybeUninit::write`. The returned owner drops the newly written value once.
    pub fn write(mut owned: Self, value: T) -> KBox<T> {
        (*owned).write(value);
        let owned = ManuallyDrop::new(owned);
        KBox {
            pointer: owned.pointer.cast(),
            owner: PhantomData,
        }
    }
}

impl<T: ?Sized> Deref for KBox<T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: construction initialized this exclusively owned slot; the
        // returned shared reference is bounded by the owner's shared borrow.
        unsafe { self.pointer.as_ref() }
    }
}

impl<T: ?Sized> DerefMut for KBox<T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: construction initialized the slot and the exclusive owner
        // borrow excludes any other references for the returned lifetime.
        unsafe { self.pointer.as_mut() }
    }
}

impl<T: ?Sized> AsRef<T> for KBox<T> {
    fn as_ref(&self) -> &T {
        self
    }
}

impl<T: ?Sized> AsMut<T> for KBox<T> {
    fn as_mut(&mut self) -> &mut T {
        self
    }
}

/// Formatting delegates to the value and retains its execution-context requirements.
impl<T: fmt::Debug + ?Sized> fmt::Debug for KBox<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, formatter)
    }
}

/// Delegation does not allocate; the iterator retains its own context requirements.
impl<T: Iterator + ?Sized> Iterator for KBox<T> {
    type Item = T::Item;

    fn next(&mut self) -> Option<Self::Item> {
        (**self).next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (**self).size_hint()
    }

    fn nth(&mut self, count: usize) -> Option<Self::Item> {
        (**self).nth(count)
    }
}

/// Reusable callbacks delegate without allocation, retaining their own context
/// requirements. Consuming this owner calls through a mutable borrow and then
/// drops it; callbacks that implement only `FnOnce` are not supported.
impl<Args: core::marker::Tuple, T: FnMut<Args> + ?Sized> FnOnce<Args> for KBox<T> {
    type Output = T::Output;

    extern "rust-call" fn call_once(mut self, args: Args) -> Self::Output {
        (*self).call_mut(args)
    }
}

impl<Args: core::marker::Tuple, T: FnMut<Args> + ?Sized> FnMut<Args> for KBox<T> {
    extern "rust-call" fn call_mut(&mut self, args: Args) -> Self::Output {
        (**self).call_mut(args)
    }
}

impl<Args: core::marker::Tuple, T: Fn<Args> + ?Sized> Fn<Args> for KBox<T> {
    extern "rust-call" fn call(&self, args: Args) -> Self::Output {
        (**self).call(args)
    }
}

impl<T: ?Sized> Drop for KBox<T> {
    fn drop(&mut self) {
        let _storage = Storage {
            pointer: self.pointer.cast(),
            layout: Layout::for_value(&**self),
        };
        // SAFETY: the slot contains one initialized T, uniquely owned here.
        // The guard retains its allocation and pre-drop layout through drop,
        // including unwinding; no reference is created after destruction.
        unsafe { self.pointer.as_ptr().drop_in_place() };
    }
}

// SAFETY: ownership can cross threads only when the owned T can; the allocator
// supports freeing on a different CPU and no references outlive the owner.
unsafe impl<T: ?Sized + Send> Send for KBox<T> {}
// SAFETY: shared access exposes only &T, and mutation requires &mut KBox<T>.
unsafe impl<T: ?Sized + Sync> Sync for KBox<T> {}

unittest::path_mod_test!(mod tests, "../ut-tests/kbox.rs");

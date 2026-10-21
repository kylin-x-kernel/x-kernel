// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{
    cell::UnsafeCell,
    marker::PhantomData,
    sync::atomic::{
        AtomicBool, AtomicI8, AtomicI16, AtomicI32, AtomicIsize, AtomicU8, AtomicU16, AtomicU32,
        AtomicUsize,
    },
};

use crate::{
    CpuArea, CpuLocalGuard, CpuPin,
    single_instr::{SingleInstructionLoad, SingleInstructionStore},
};

/// Marks vetted types as bitwise-copyable into every CPU area.
///
/// # Safety
/// Each type's *template state* (its `::new()` value) must be copy-safe —
/// typically a no-allocation marker such as `LazyInit::new()`, `Vec::new()`,
/// or `Weak::new()` — and the per-CPU slot storage must never drop slot
/// values: drops happen only through owner-code assignment or removal (an
/// in-place `Weak` overwrite dropping the previous weak, a `pop_front`
/// dropping a deque entry), never through storage teardown. Atomic types
/// qualify without being `Copy` for the same reason. Types with ownership
/// or self-reference semantics that cannot honor this must use
/// [`crate::DynamicCpuSlot`] so each CPU constructs an independent value.
#[macro_export]
macro_rules! impl_static_slot_value {
    (unsafe $($ty:ty),+ $(,)?) => {$(
        // SAFETY: the caller vouches for the bitwise-copy contract above.
        unsafe impl $crate::StaticSlotValue for $ty {}
    )+};
}

/// Types whose object representation may be copied into every CPU area.
/// `Sync` is intentionally not required: callers establish per-CPU exclusion
/// with a [`CpuLocalGuard`] before accessing a slot.
///
/// Implementations are unsafe because the compiler cannot verify this
/// template-copy contract. Every external implementation must include a
/// concrete `SAFETY` argument at the implementation site. Types with
/// ownership, drop, reference, or self-reference semantics must use
/// [`crate::DynamicCpuSlot`] so each CPU constructs an independent value
/// instead of receiving a bytewise copy.
///
/// # Safety
/// Every value used as a static slot template for the implementing type must
/// remain valid when copied bitwise into independent, non-dropping CPU areas.
pub unsafe trait StaticSlotValue: 'static {}

crate::impl_static_slot_value!(
    unsafe
    bool,
    u8,
    u16,
    u32,
    u64,
    usize,
    i8,
    i16,
    i32,
    i64,
    isize,
    AtomicBool,
    AtomicU8,
    AtomicI8,
    AtomicU16,
    AtomicI16,
    AtomicU32,
    AtomicI32,
    AtomicUsize,
    AtomicIsize,
);

// SAFETY: an array of bitwise-copyable members is bitwise-copyable.
unsafe impl<T: StaticSlotValue, const N: usize> StaticSlotValue for [T; N] {}

// SAFETY: `Option::None` is the allocation-free template state and `Some`
// carries an already-vetted member by value; the storage never drops it.
unsafe impl<T: StaticSlotValue> StaticSlotValue for Option<T> {}

// SAFETY: x86-64 descriptor/state tables are plain, drop-free structs whose
// `::new()` templates are all-zero bits; each CPU fills its own copy in place
// and the storage never moves or drops them.
#[cfg(target_arch = "x86_64")]
unsafe impl StaticSlotValue for x86_64::structures::tss::TaskStateSegment {}

#[cfg(target_arch = "x86_64")]
// SAFETY: the descriptor table is a plain fixed-size entry array without
// drop; each CPU appends its descriptors in place and never moves it.
unsafe impl StaticSlotValue for x86_64::structures::gdt::GlobalDescriptorTable {}

/// A statically allocated per-CPU slot.
#[repr(C)]
pub struct CpuSlot<T: 'static> {
    offset_fn: fn() -> usize,
    _marker: PhantomData<fn() -> T>,
}

impl<T: 'static> CpuSlot<T> {
    /// Builds a descriptor from a linker-offset function produced by
    /// [`crate::cpu_slot!`].
    ///
    /// # Safety
    /// `offset_fn` must be the `offset` generated for a `.cpu_slot.template`
    /// symbol by [`crate::cpu_slot!`]; the linker layout must have been
    /// initialized with [`crate::begin_cpu_init`]. An arbitrary offset_fn could
    /// name a memory region outside the CPU area.
    #[doc(hidden)]
    pub const unsafe fn from_offset_fn(offset_fn: fn() -> usize) -> Self {
        Self {
            offset_fn,
            _marker: PhantomData,
        }
    }

    fn offset(&self) -> usize {
        (self.offset_fn)()
    }

    /// Accesses this CPU's shared value with ordinary IRQs temporarily masked.
    ///
    /// The slot area must already be initialized. The callback must not block,
    /// schedule, change the CPU base, or enable ordinary IRQs. The original IRQ
    /// mask is restored on return or unwind, without preemption accounting or
    /// scheduler checks. NMI is not excluded; `T: Sync` remains required.
    /// The borrowed slot cannot escape the callback.
    #[inline]
    pub fn with_current_irq_disabled<R>(&self, access_fn: impl for<'a> FnOnce(&'a T) -> R) -> R
    where
        T: Sync,
    {
        let _irq = crate::guard::LocalIrqGuard::new();
        access_fn(self.current_shared())
    }

    /// Shares the pinned CPU's value when the payload is intrinsically safe
    /// for concurrent shared access, such as an atomic value.
    pub fn current<'a>(&'a self, pin: &'a impl CpuPin) -> &'a T
    where
        T: Sync,
    {
        // SAFETY: `CpuPin` selects an initialized current area for `'a`; `T:
        // Sync` permits concurrent shared access from IRQ/NMI contexts.
        unsafe { self.get_unchecked(pin) }
    }

    /// Shares a value in a checked, published CPU area.
    pub fn at<'a>(&'a self, area: &'a CpuArea) -> &'a T
    where
        T: Sync,
    {
        // SAFETY: `CpuArea` is created only after initialization is published,
        // and `T: Sync` permits concurrent shared access.
        unsafe { &*((area.base() + self.offset()) as *const T) }
    }

    /// Shares the current CPU's value through a guard that excludes local IRQ
    /// reentry.
    ///
    /// The guard's guarantee makes any [`StaticSlotValue`] — `Sync` or not —
    /// safe to share for the guard's lifetime: no other context on this CPU
    /// can run to observe the slot, and the context cannot migrate to
    /// another CPU's instance.
    pub fn get<'a>(&'a self, guard: &'a impl CpuLocalGuard) -> &'a T {
        // SAFETY: The offset function names a linker-retained template symbol
        // and `pin.base()` names the pinned CPU's initialized area. The guard
        // prevents migration and local IRQ reentry for the duration of `'a`,
        // so no conflicting access can overlap the returned reference.
        unsafe { self.get_unchecked(guard) }
    }

    /// Shares the current CPU's value without a guard.
    ///
    /// Sound only because `T: Sync` and the slot storage is permanent. The
    /// reference names the CPU selected by the base register at the call, so
    /// self-hosting code that runs before any guard exists (CPU identity, the
    /// active-trapframe atomic) can use it. Ordinary code should prefer
    /// [`Self::get`], which additionally bounds the borrow by a guard.
    pub fn current_shared(&self) -> &T
    where
        T: Sync,
    {
        // SAFETY: the base register selects this CPU's initialized area, the
        // storage is permanent, and `T: Sync` permits shared access.
        unsafe { &*((crate::arch::current_base() + self.offset()) as *const T) }
    }

    /// Shares the current CPU's value through a plain [`CpuPin`].
    ///
    /// # Safety
    /// See [`Self::get`]; the caller must additionally prove that no local
    /// IRQ handler can access this slot while the returned reference is
    /// alive, since a plain pin does not exclude reentry.
    pub(crate) unsafe fn get_unchecked<'a>(&'a self, pin: &'a impl CpuPin) -> &'a T {
        // SAFETY: The offset function names a linker-retained template symbol,
        // `pin.base()` names the pinned CPU's initialized area, and the caller
        // guarantees migration is prevented for the duration of `'a`.
        unsafe { &*((pin.base() + self.offset()) as *const T) }
    }

    /// Exclusively accesses the current CPU's value through a plain
    /// [`CpuPin`].
    ///
    /// # Safety
    /// The caller must provide exclusive access to this CPU's slot for the
    /// duration of the returned reference. Two references obtained through
    /// guards of any kind without an intervening synchronization point alias
    /// and are not mutually exclusive.
    #[cfg(test)]
    #[allow(clippy::mut_from_ref)]
    pub(crate) unsafe fn get_mut_unchecked<'a>(&'a self, pin: &'a impl CpuPin) -> &'a mut T {
        // SAFETY: See [`Self::get_unchecked`]; exclusive access is supplied
        // by caller.
        unsafe { &mut *((pin.base() + self.offset()) as *mut T) }
    }
}

/// Interior-mutable per-CPU slot for local state.
#[repr(C)]
pub struct CpuSlotCell<T: 'static> {
    offset_fn: fn() -> usize,
    _not_send_sync: PhantomData<*mut ()>,
    _marker: PhantomData<fn() -> T>,
}

// SAFETY: Sharing `&CpuSlotCell<T>` does not expose a value directly: the
// descriptor holds only an offset function and phantom markers, and every
// accessor is either `unsafe` (caller proves CPU-local exclusivity) or gated
// on `T: Sync`. The `T: 'static` bound is all the descriptor itself needs;
// template-copy safety is certified separately at the `cpu_slot_cell!` /
// `cpu_slot_cell_with_template!` invocation site.
unsafe impl<T: 'static> Sync for CpuSlotCell<T> {}

impl<T: 'static> CpuSlotCell<T> {
    /// Builds a descriptor from a linker-offset function produced by
    /// [`crate::cpu_slot_cell!`].
    ///
    /// # Safety
    /// `offset_fn` must be the `offset` generated for a `.cpu_slot.template`
    /// symbol by [`crate::cpu_slot_cell!`]; the linker layout must have been
    /// initialized with [`crate::begin_cpu_init`]. An arbitrary offset_fn could
    /// name a memory region outside the CPU area.
    #[doc(hidden)]
    pub const unsafe fn from_offset_fn(offset_fn: fn() -> usize) -> Self {
        Self {
            offset_fn,
            _not_send_sync: PhantomData,
            _marker: PhantomData,
        }
    }

    fn offset(&self) -> usize {
        (self.offset_fn)()
    }

    /// Shares a value in a checked, published CPU area when the payload
    /// provides its own synchronization.
    pub fn shared_at(&self, area: CpuArea) -> &T
    where
        T: Sync,
    {
        // SAFETY: `CpuArea` proves that the storage was initialized and
        // published. `T: Sync` permits concurrent shared access; mutation, if
        // any, must be mediated by the payload's atomics or locks.
        unsafe { &*((area.base() + self.offset()) as *const T) }
    }

    /// Shares the current CPU's cell value without a guard.
    ///
    /// Sound only because `T: Sync` and the storage is permanent. Used by
    /// self-hosting code that cannot construct a guard (CPU identity, active
    /// trapframe atomics); ordinary non-`Sync` payloads must use
    /// [`Self::get`]/[`Self::get_mut`].
    pub fn shared_current(&self) -> &T
    where
        T: Sync,
    {
        // SAFETY: the base register selects this CPU's initialized area, the
        // storage is permanent, and `T: Sync` permits shared access.
        unsafe { &*((crate::arch::current_base() + self.offset()) as *const T) }
    }

    /// Loads the current CPU's scalar through a single-instruction operation.
    pub fn load_current(&self) -> T
    where
        T: SingleInstructionLoad,
    {
        // SAFETY: the descriptor was produced by `cpu_slot_cell!`, so `offset`
        // names a live per-CPU slot of this exact type.
        unsafe { T::load(self.offset()) }
    }

    /// Stores the current CPU's scalar through a single-instruction operation.
    pub fn store_current(&self, value: T)
    where
        T: SingleInstructionStore,
    {
        // SAFETY: as `load_current`; the write is a single per-CPU operation.
        unsafe { T::store(self.offset(), value) }
    }

    /// Shares the pinned CPU's value for the guard's lifetime.
    ///
    /// The guard excludes migration and local IRQ reentry, so the reference
    /// cannot outlive the context that owns this CPU's slot. `Sync` is not
    /// required: mutation must still go through the payload's own interior
    /// mutability (atomics or a lock) or through the unsafe
    /// [`Self::get_mut_unchecked`].
    pub fn get<'a>(&'a self, guard: &'a impl CpuLocalGuard) -> &'a T {
        // SAFETY: the guard proves the pinned CPU's area is initialized and
        // that no conflicting local access can overlap `'a`, so a shared
        // reference is valid even for non-`Sync` payloads.
        let cell = unsafe { self.get_unchecked(guard) };
        // SAFETY: The guard keeps this CPU pinned and excludes conflicting local
        // access throughout the shared borrow of the initialized value.
        unsafe { &*cell.get() }
    }

    /// Exclusively borrows the pinned CPU's value through a plain [`CpuPin`].
    ///
    /// # Safety
    /// A plain pin does not exclude local IRQ reentry, and a
    /// [`CpuLocalGuard`] bounds only its own borrow: two different guards can
    /// both name this slot. The caller must therefore hold exclusive access to
    /// this CPU's value for the returned lifetime.
    #[expect(
        clippy::mut_from_ref,
        reason = "CG-024-lint-exceptions: clippy::mut_from_ref: The unsafe caller guarantees \
                  exclusive access to the pinned CPU slot for the returned lifetime"
    )]
    pub unsafe fn get_mut_unchecked<'a>(&'a self, pin: &'a impl CpuPin) -> &'a mut T {
        // SAFETY: see [`Self::get_unchecked`]; exclusive access is supplied by
        // the caller.
        let cell = unsafe { self.get_unchecked(pin) };
        // SAFETY: The caller guarantees exclusive access to the initialized CPU
        // value for the returned lifetime; pin keeps its address stable.
        unsafe { &mut *cell.get() }
    }

    /// Accesses the current CPU's cell.
    ///
    /// # Safety
    /// The slot must be initialized, the caller must not migrate off the pinned
    /// CPU while the returned cell is alive, and the caller must ensure no
    /// conflicting access to the selected CPU's value.
    pub(crate) unsafe fn get_unchecked<'a>(&'a self, pin: &'a impl CpuPin) -> &'a UnsafeCell<T> {
        // SAFETY: `pin.base()` names an area installed by `begin_cpu_init` for
        // the pinned CPU. The descriptor was generated by `cpu_slot_cell!`,
        // whose hidden template symbol is retained in the linker layout; every
        // CPU copy has the same size and alignment. Therefore the resulting
        // address is initialized and aligned for `UnsafeCell<T>`, while the
        // caller's pin/exclusivity contract prevents conflicting access.
        unsafe { &*((pin.base() + self.offset()) as *const UnsafeCell<T>) }
    }

    /// Copies out the current CPU's cell value.
    ///
    /// By-value access escapes no reference, so a guard that excludes local
    /// IRQ reentry is enough for soundness even for non-`Sync` `T`.
    /// The copy is a plain load; this is not an atomic read.
    pub fn read(&self, guard: &impl CpuLocalGuard) -> T
    where
        T: Copy,
    {
        // SAFETY: The guard pins this CPU and excludes local IRQ reentry for
        // the duration of the call, so the cell is initialized and not
        // concurrently accessed; no reference escapes the call.
        let cell = unsafe { self.get_unchecked(guard) };
        // SAFETY: The initialized cell remains pinned, and the access exclusion
        // described above covers this entire copy.
        unsafe { cell.get().read() }
    }

    /// Overwrites the current CPU's cell value.
    ///
    /// By-value access escapes no reference, so a guard that excludes local
    /// IRQ reentry is enough for soundness even for non-`Sync` `T`.
    /// The store is a plain write; this is not an atomic write.
    pub fn write(&self, guard: &impl CpuLocalGuard, value: T) {
        // SAFETY: The guard pins this CPU and excludes local IRQ reentry for
        // the duration of the call, so the cell is initialized and no other
        // context can observe the store in progress.
        let cell = unsafe { self.get_unchecked(guard) };
        // SAFETY: The initialized cell remains pinned, and the access exclusion
        // described above covers this entire write.
        unsafe { cell.get().write(value) }
    }

    /// Copies out the current CPU's cell value through a plain
    /// [`CpuPin`].
    ///
    /// # Safety
    /// See [`Self::get_unchecked`]; the caller must prove that no local IRQ
    /// handler can access this cell during the call.
    #[cfg(test)]
    pub(crate) unsafe fn read_unchecked(&self, pin: &impl CpuPin) -> T
    where
        T: Copy,
    {
        // SAFETY: The caller's pin contract guarantees the cell is
        // initialized and that no conflicting access runs during the call.
        let cell = unsafe { self.get_unchecked(pin) };
        // SAFETY: The initialized cell remains pinned, and the access exclusion
        // described above covers this entire copy.
        unsafe { cell.get().read() }
    }

    /// Overwrites the current CPU's cell value through a plain
    /// [`CpuPin`].
    ///
    /// # Safety
    /// See [`Self::get_unchecked`]; the caller must prove exclusive access to
    /// the selected CPU's value for the duration of the operation.
    #[cfg(test)]
    pub(crate) unsafe fn write_unchecked(&self, pin: &impl CpuPin, value: T) {
        // SAFETY: The caller's pin contract guarantees exclusive access to
        // the selected CPU's cell for the duration of the store.
        let cell = unsafe { self.get_unchecked(pin) };
        // SAFETY: The initialized cell remains pinned, and the access exclusion
        // described above covers this entire write.
        unsafe { cell.get().write(value) }
    }

    /// Mutably borrows the current CPU's value for the kernel lifetime.
    ///
    /// This is the one guard-free raw primitive, reserved for x86 self-hosting
    /// bootstrapping whose upstream API demands a `'static` reference (loading
    /// the GDT). Everything else must use a guard-bounded accessor.
    ///
    /// # Safety
    /// The caller must be the single execution context on this CPU and must
    /// not alias the value for the returned lifetime.
    pub unsafe fn current_shared_mut(&self) -> &'static mut T {
        // SAFETY: caller's contract; the slot storage is permanent.
        unsafe { &mut *((crate::arch::current_base() + self.offset()) as *mut T) }
    }
}

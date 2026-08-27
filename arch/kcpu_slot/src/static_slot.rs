// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{cell::UnsafeCell, marker::PhantomData};

use crate::PinCurrentCpu;

mod sealed {
    pub trait StaticSlotValue {}
}

/// Types whose object representation may be copied into every CPU area.
/// `Sync` is intentionally not required: callers establish per-CPU exclusion
/// with [`PinCurrentCpu`] before accessing a slot.
///
/// The trait is sealed intentionally. Types with ownership, drop, reference,
/// or self-reference semantics must use [`crate::DynamicCpuSlot`] so each CPU
/// constructs an independent value instead of receiving a bytewise copy.
pub trait StaticSlotValue: sealed::StaticSlotValue + Copy + Send + 'static {}

macro_rules! impl_static_slot_value {
    ($($ty:ty),+ $(,)?) => {
        $(
            impl sealed::StaticSlotValue for $ty {}
            impl StaticSlotValue for $ty {}
        )+
    };
}

impl_static_slot_value!(bool, u8, u16, u32, u64, usize, i8, i16, i32, i64, isize);

impl<T: StaticSlotValue, const N: usize> sealed::StaticSlotValue for [T; N] {}
impl<T: StaticSlotValue, const N: usize> StaticSlotValue for [T; N] {}

/// A statically allocated per-CPU slot.
#[repr(C)]
pub struct CpuSlot<T: StaticSlotValue> {
    offset_fn: fn() -> usize,
    _marker: PhantomData<fn() -> T>,
}

impl<T: StaticSlotValue> CpuSlot<T> {
    /// Builds a descriptor from a linker-offset function produced by
    /// [`crate::cpu_slot!`].
    ///
    /// # Safety
    /// `offset_fn` must be the `offset` generated for a `.cpu_slot.template`
    /// symbol by [`crate::cpu_slot!`]; the linker layout must have been
    /// initialized with [`crate::initialize_cpu`]. An arbitrary offset_fn could
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

    /// Reads the current CPU's value through a pinned execution context.
    ///
    /// # Safety
    /// `self` must be a descriptor created by [`crate::cpu_slot!`], the linker
    /// layout must have been initialized with [`crate::initialize_cpu`], and the
    /// caller must not migrate off the pinned CPU while the returned reference
    /// is alive.
    pub unsafe fn get<'a>(&'a self, pin: &'a impl PinCurrentCpu) -> &'a T {
        // SAFETY: The offset function names a linker-retained template symbol,
        // `pin.base()` names the pinned CPU's initialized area, and the caller
        // guarantees migration is prevented for the duration of `'a`.
        unsafe { &*((pin.base() + self.offset()) as *const T) }
    }

    /// Mutably accesses the current CPU's value.
    ///
    /// # Safety
    /// The caller must provide exclusive access to this CPU's slot for the
    /// duration of the returned reference. Two references obtained through the
    /// same pin without an intervening synchronization point alias and are not
    /// mutually exclusive.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn get_mut<'a>(&'a self, pin: &'a impl PinCurrentCpu) -> &'a mut T {
        // SAFETY: See [`Self::get`]; exclusive access is supplied by caller.
        unsafe { &mut *((pin.base() + self.offset()) as *mut T) }
    }

    /// Accesses a slot in an explicitly supplied, valid CPU area.
    ///
    /// # Safety
    /// `base` must point to an initialized area belonging to the selected CPU,
    /// and the caller must ensure the returned reference does not race with any
    /// other access to that CPU's value.
    pub unsafe fn get_at(&self, base: usize) -> &T
    where
        T: Sync,
    {
        // SAFETY: The caller supplies a valid initialized CPU area and the
        // `Sync` bound excludes shared access to non-`Sync` values.
        unsafe { &*((base + self.offset()) as *const T) }
    }
}

/// Interior-mutable per-CPU slot for local state.
#[repr(C)]
pub struct CpuSlotCell<T: StaticSlotValue> {
    offset_fn: fn() -> usize,
    _not_send_sync: PhantomData<*mut ()>,
    _marker: PhantomData<fn() -> T>,
}

// SAFETY: Sharing `&CpuSlotCell<T>` does not expose a value directly. Access
// requires a `PinCurrentCpu`, whose caller must establish CPU-local
// exclusivity; `T: Send` ensures the contained value may be placed in a
// per-CPU area during initialization without violating ownership rules.
unsafe impl<T: StaticSlotValue> Sync for CpuSlotCell<T> {}

impl<T: StaticSlotValue> CpuSlotCell<T> {
    /// Builds a descriptor from a linker-offset function produced by
    /// [`crate::cpu_slot_cell!`].
    ///
    /// # Safety
    /// `offset_fn` must be the `offset` generated for a `.cpu_slot.template`
    /// symbol by [`crate::cpu_slot_cell!`]; the linker layout must have been
    /// initialized with [`crate::initialize_cpu`]. An arbitrary offset_fn could
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

    /// Accesses the current CPU's cell.
    ///
    /// # Safety
    /// The slot must be initialized, the caller must not migrate off the pinned
    /// CPU while the returned cell is alive, and the caller must ensure no
    /// conflicting access to the selected CPU's value.
    pub unsafe fn get<'a>(&'a self, pin: &'a impl PinCurrentCpu) -> &'a UnsafeCell<T> {
        // SAFETY: `pin.base()` names an area installed by `initialize_cpu` for
        // the pinned CPU. The descriptor was generated by `cpu_slot_cell!`,
        // whose hidden template symbol is retained in the linker layout; every
        // CPU copy has the same size and alignment. Therefore the resulting
        // address is initialized and aligned for `UnsafeCell<T>`, while the
        // caller's pin/exclusivity contract prevents conflicting access.
        unsafe { &*((pin.base() + self.offset()) as *const UnsafeCell<T>) }
    }

    /// Reads the current CPU's cell value.
    ///
    /// # Safety
    /// See [`Self::get`].
    pub unsafe fn read<'a>(&'a self, pin: &'a impl PinCurrentCpu) -> T
    where
        T: Copy,
    {
        unsafe { *self.get(pin).get() }
    }

    /// Writes to the current CPU's cell.
    ///
    /// # Safety
    /// See [`Self::get`]; the caller must establish exclusive access to the
    /// selected CPU's value for the duration of the operation.
    pub unsafe fn write<'a>(&'a self, pin: &'a impl PinCurrentCpu, value: T) {
        unsafe { *self.get(pin).get() = value };
    }
}

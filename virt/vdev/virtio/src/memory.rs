// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Scoped guest-memory operations used by VirtIO queues.

use core::mem::size_of;

use crate::{GuestPhysAddr, PhysAddr, VirtioError, VirtioResult};

/// Error returned by a guest-memory accessor when a translation or access
/// fails or would cross the translated region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryAccessError {
    /// The guest address could not be translated to host memory.
    Unmapped,
    /// The access crosses the contiguous translation limit.
    OutOfLimits,
}

/// The minimum guest-memory capability required by a VirtIO queue operation.
pub trait GuestMemory {
    /// Reads bytes starting at `guest_addr`.
    fn read(&mut self, guest_addr: GuestPhysAddr, data: &mut [u8]) -> VirtioResult<()>;

    /// Writes bytes starting at `guest_addr`.
    fn write(&mut self, guest_addr: GuestPhysAddr, data: &[u8]) -> VirtioResult<()>;
}

/// Address-space accessor capability required by the `VirtioQueue<T>`
/// configuration setters and the [`AddressSpaceMemory`] adapter.
///
/// This is the crate-local equivalent of the ArceOS `axaddrspace`
/// `GuestMemoryAccessor` trait the VirtIO code was written against. A VMM that
/// exposes a directly-dereferenceable host mapping implements it; a runtime
/// that only holds guest memory as a scoped capability at queue-processing time
/// uses [`NoGuestMemoryAccessor`] plus the `_with_memory` operations, backed by
/// a `vdev_core::GuestDma`-based [`crate::GuestMemory`] instead.
pub trait GuestMemoryAccessor: Clone {
    /// Translate a guest physical address to a host physical address and the
    /// contiguous guest bytes available at that translation.
    ///
    /// The returned host address must be directly dereferenceable by the caller
    /// (KVMM: `gpa_to_hpa` followed by `kaddr_layout::p2v`; host tests: a
    /// plain pointer into a test buffer) — the default [`read_buffer`]
    /// / [`write_buffer`] implementations copy through it.
    ///
    /// [`read_buffer`]: Self::read_buffer
    /// [`write_buffer`]: Self::write_buffer
    fn translate_and_get_limit(&self, guest_addr: GuestPhysAddr) -> Option<(PhysAddr, usize)>;

    /// Copy `data.len()` bytes from guest memory starting at `guest_addr`.
    fn read_buffer(
        &self,
        guest_addr: GuestPhysAddr,
        data: &mut [u8],
    ) -> Result<(), MemoryAccessError> {
        let (host_addr, limit) = self
            .translate_and_get_limit(guest_addr)
            .ok_or(MemoryAccessError::Unmapped)?;
        if limit < data.len() {
            return Err(MemoryAccessError::OutOfLimits);
        }
        // SAFETY: `translate_and_get_limit` promises a dereferenceable host
        // address for at least `limit` guest bytes; the length is checked above.
        unsafe {
            core::ptr::copy_nonoverlapping(
                host_addr.as_usize() as *const u8,
                data.as_mut_ptr(),
                data.len(),
            );
        }
        Ok(())
    }

    /// Copy `data.len()` bytes into guest memory starting at `guest_addr`.
    fn write_buffer(
        &self,
        guest_addr: GuestPhysAddr,
        data: &[u8],
    ) -> Result<(), MemoryAccessError> {
        let (host_addr, limit) = self
            .translate_and_get_limit(guest_addr)
            .ok_or(MemoryAccessError::Unmapped)?;
        if limit < data.len() {
            return Err(MemoryAccessError::OutOfLimits);
        }
        // SAFETY: same contract as `read_buffer`.
        unsafe {
            core::ptr::copy_nonoverlapping(
                data.as_ptr(),
                host_addr.as_usize() as *mut u8,
                data.len(),
            );
        }
        Ok(())
    }

    /// Read a `Sized` value from guest memory.
    ///
    /// # Safety
    ///
    /// `T` must be a plain-old-data type with a byte-addressable layout and no
    /// invalid bit patterns, padding invariants, or owned pointers: every byte
    /// read from guest memory is reinterpreted as a live `T` via
    /// [`MaybeUninit::assume_init`](core::mem::MaybeUninit::assume_init). VirtIO
    /// ring headers (`u8`/`u16`/`u32`/`u64` and the `#[repr(C)]` header structs)
    /// satisfy this; types with pointers, references, enums with uninhabited
    /// variants, or `bool` do not.
    unsafe fn read_obj<T: Sized>(&self, guest_addr: GuestPhysAddr) -> Result<T, MemoryAccessError> {
        let mut value = core::mem::MaybeUninit::<T>::uninit();
        // SAFETY: `T` is POD per the caller's contract, so viewing its storage
        // as `size_of::<T>()` bytes is valid; `read_buffer` fills every byte
        // before `assume_init` below.
        let bytes = unsafe {
            core::slice::from_raw_parts_mut(value.as_mut_ptr() as *mut u8, size_of::<T>())
        };
        self.read_buffer(guest_addr, bytes)?;
        // SAFETY: all `size_of::<T>()` bytes were just initialized, and `T` is
        // POD per the caller's contract so any bit pattern is a valid `T`.
        Ok(unsafe { value.assume_init() })
    }

    /// Write a `Sized` value into guest memory.
    ///
    /// # Safety
    ///
    /// `T` must be a plain-old-data type: it is reinterpreted as
    /// `size_of::<T>()` raw bytes. Padding bytes are copied as-is, so a type
    /// with uninitialized padding leaks those bytes into guest memory but does
    /// not cause UB; types with pointers or references must not be passed.
    unsafe fn write_obj<T: Sized>(
        &self,
        guest_addr: GuestPhysAddr,
        value: T,
    ) -> Result<(), MemoryAccessError> {
        // SAFETY: reading the live `value` as `size_of::<T>()` bytes is valid for
        // a POD `T` per the caller's contract.
        let bytes =
            unsafe { core::slice::from_raw_parts(&raw const value as *const u8, size_of::<T>()) };
        self.write_buffer(guest_addr, bytes)
    }
}

/// Placeholder accessor for runtimes that provide guest memory only through
/// a scoped [`GuestMemory`] capability at queue-processing time.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoGuestMemoryAccessor;

impl GuestMemoryAccessor for NoGuestMemoryAccessor {
    fn translate_and_get_limit(&self, _guest_addr: GuestPhysAddr) -> Option<(PhysAddr, usize)> {
        None
    }
}

/// Adapter for existing address-space accessors used by host tests and
/// standalone device-model users.
pub struct AddressSpaceMemory<'a, T> {
    accessor: &'a T,
}

impl<'a, T> AddressSpaceMemory<'a, T> {
    /// Wraps one address-space accessor for a scoped queue operation.
    pub const fn new(accessor: &'a T) -> Self {
        Self { accessor }
    }
}

impl<T: GuestMemoryAccessor> GuestMemory for AddressSpaceMemory<'_, T> {
    fn read(&mut self, guest_addr: GuestPhysAddr, data: &mut [u8]) -> VirtioResult<()> {
        self.accessor
            .read_buffer(guest_addr, data)
            .map_err(|_| VirtioError::InvalidAddress)
    }

    fn write(&mut self, guest_addr: GuestPhysAddr, data: &[u8]) -> VirtioResult<()> {
        self.accessor
            .write_buffer(guest_addr, data)
            .map_err(|_| VirtioError::InvalidAddress)
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! MMIO resource types: region/port description, mapping token, the [`MmioOp`]
//! capability trait, and the [`Io`] RAII handle for register access.

use core::{
    ptr::NonNull,
    sync::atomic::{Ordering, fence},
};

use crate::ResResult;

/// A memory-mapped I/O region described by physical address and size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MmioRegion {
    /// Physical base address.
    pub base: usize,
    /// Region size in bytes.
    pub size: usize,
}

/// An x86 I/O port range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IoPortRange {
    /// Base port number.
    pub base: u16,
    /// Number of consecutive ports.
    pub size: u16,
}

/// A mapping token returned by [`MmioOp::map_mmio`].
///
/// The provider is responsible for interpreting the token in
/// [`MmioOp::unmap_mmio`]. Drivers never construct this directly; they hold an
/// [`Io`] handle instead.
#[derive(Debug, Clone, Copy)]
pub struct MmioMapping {
    /// Virtual address the CPU uses to access the region.
    pub vaddr: usize,
    /// The physical region this mapping covers.
    pub region: MmioRegion,
}

/// MMIO mapping capability: map a physical device region into CPU-visible
/// virtual memory and release it.
///
/// A host kernel implements this and passes it to its driver framework. Runs
/// in normal (non-interrupt) context during device probe and removal.
pub trait MmioOp: Sync {
    /// Map an MMIO region and return a token for later teardown.
    fn map_mmio(&self, region: MmioRegion, name: &'static str) -> ResResult<MmioMapping>;

    /// Release a mapping previously returned by [`Self::map_mmio`].
    fn unmap_mmio(&self, mapping: MmioMapping);
}

/// RAII handle to a mapped MMIO region.
///
/// Dropping the handle releases the mapping through the provider that created
/// it.
pub struct Io {
    provider: Option<&'static dyn MmioOp>,
    mapping: Option<MmioMapping>,
}

impl Io {
    /// Map an MMIO region with an explicit provider.
    pub fn map_with(
        provider: &'static dyn MmioOp,
        region: MmioRegion,
        name: &'static str,
    ) -> ResResult<Self> {
        let mapping = provider.map_mmio(region, name)?;
        Ok(Self {
            provider: Some(provider),
            mapping: Some(mapping),
        })
    }

    /// The virtual base address of the mapping.
    ///
    /// # Panics
    ///
    /// Panics if the handle has no active mapping.
    pub fn as_ptr(&self) -> NonNull<u8> {
        NonNull::new(
            self.mapping
                .as_ref()
                .expect("Io handle used after release")
                .vaddr as *mut u8,
        )
        .expect("Io mapping stored a null virtual address")
    }

    /// The physical region backing this mapping.
    ///
    /// # Panics
    ///
    /// Panics if the handle has no active mapping.
    pub fn region(&self) -> MmioRegion {
        self.mapping
            .as_ref()
            .expect("Io handle used after release")
            .region
    }

    /// Returns a checked pointer `offset` bytes into the region, asserting that
    /// `[offset, offset + size)` stays within bounds.
    #[inline]
    fn access_ptr(&self, offset: usize, size: usize) -> *mut u8 {
        let region = self.region();
        let end = offset
            .checked_add(size)
            .expect("MMIO access offset overflow");
        assert!(end <= region.size, "MMIO access out of bounds");
        // SAFETY: the offset has been bounds-checked against the mapped region
        // length, so the resulting pointer stays inside the mapping.
        unsafe { self.as_ptr().as_ptr().add(offset) }
    }

    /// Read a `u8` register at `offset`.
    #[inline]
    pub fn read8(&self, offset: usize) -> u8 {
        let ptr = self.access_ptr(offset, 1);
        // SAFETY: `access_ptr` bounds-checked the access; `u8` has no alignment
        // requirement.
        let value = unsafe { ptr.read_volatile() };
        fence(Ordering::Acquire);
        value
    }

    /// Read a `u16` register at `offset` (must be 2-byte aligned).
    #[inline]
    pub fn read16(&self, offset: usize) -> u16 {
        let ptr = self.access_ptr(offset, 2) as *const u16;
        debug_assert!((ptr as usize).is_multiple_of(2), "unaligned MMIO u16 read");
        // SAFETY: bounds- and alignment-checked above.
        let value = unsafe { ptr.read_volatile() };
        fence(Ordering::Acquire);
        value
    }

    /// Read a `u32` register at `offset` (must be 4-byte aligned).
    #[inline]
    pub fn read32(&self, offset: usize) -> u32 {
        let ptr = self.access_ptr(offset, 4) as *const u32;
        debug_assert!((ptr as usize).is_multiple_of(4), "unaligned MMIO u32 read");
        // SAFETY: bounds- and alignment-checked above.
        let value = unsafe { ptr.read_volatile() };
        fence(Ordering::Acquire);
        value
    }

    /// Read a `u64` register at `offset` (must be 8-byte aligned).
    #[inline]
    pub fn read64(&self, offset: usize) -> u64 {
        let ptr = self.access_ptr(offset, 8) as *const u64;
        debug_assert!((ptr as usize).is_multiple_of(8), "unaligned MMIO u64 read");
        // SAFETY: bounds- and alignment-checked above.
        let value = unsafe { ptr.read_volatile() };
        fence(Ordering::Acquire);
        value
    }

    /// Write a `u8` register at `offset`.
    #[inline]
    pub fn write8(&self, offset: usize, value: u8) {
        let ptr = self.access_ptr(offset, 1);
        fence(Ordering::Release);
        // SAFETY: `access_ptr` bounds-checked the access; `u8` has no alignment
        // requirement.
        unsafe { ptr.write_volatile(value) };
    }

    /// Write a `u16` register at `offset` (must be 2-byte aligned).
    #[inline]
    pub fn write16(&self, offset: usize, value: u16) {
        let ptr = self.access_ptr(offset, 2) as *mut u16;
        debug_assert!((ptr as usize).is_multiple_of(2), "unaligned MMIO u16 write");
        fence(Ordering::Release);
        // SAFETY: bounds- and alignment-checked above.
        unsafe { ptr.write_volatile(value) };
    }

    /// Write a `u32` register at `offset` (must be 4-byte aligned).
    #[inline]
    pub fn write32(&self, offset: usize, value: u32) {
        let ptr = self.access_ptr(offset, 4) as *mut u32;
        debug_assert!((ptr as usize).is_multiple_of(4), "unaligned MMIO u32 write");
        fence(Ordering::Release);
        // SAFETY: bounds- and alignment-checked above.
        unsafe { ptr.write_volatile(value) };
    }

    /// Write a `u64` register at `offset` (must be 8-byte aligned).
    #[inline]
    pub fn write64(&self, offset: usize, value: u64) {
        let ptr = self.access_ptr(offset, 8) as *mut u64;
        debug_assert!((ptr as usize).is_multiple_of(8), "unaligned MMIO u64 write");
        fence(Ordering::Release);
        // SAFETY: bounds- and alignment-checked above.
        unsafe { ptr.write_volatile(value) };
    }
}

impl Drop for Io {
    fn drop(&mut self) {
        if let (Some(mapping), Some(provider)) = (self.mapping.take(), self.provider.take()) {
            provider.unmap_mmio(mapping);
        }
    }
}

impl core::fmt::Debug for Io {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Io")
            .field("mapping", &self.mapping)
            .finish_non_exhaustive()
    }
}

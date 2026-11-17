// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The MMIO family: the mapping handle (`devm_ioremap` ownership).

use core::{
    fmt, ptr,
    sync::atomic::{Ordering, fence},
};

use crate::ResResult;

/// A mapped MMIO region with checked volatile accessors.
///
/// Auto-unmapped on drop. The accessors carry release/acquire fences and
/// bounds checks. Architecture-specific device ordering remains a host obligation.
pub struct IoMap {
    vaddr: usize,
    region: MmioRegion,
    provider: &'static dyn MmioOp,
}

impl fmt::Debug for IoMap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IoMap")
            .field("region", &self.region)
            .finish_non_exhaustive()
    }
}

impl IoMap {
    /// Acquire one mapping and retain its original provider for release.
    pub fn map_with(
        provider: &'static dyn MmioOp,
        region: MmioRegion,
        name: &'static str,
    ) -> ResResult<Self> {
        let mapping = provider.map_mmio(region, name)?;
        Ok(Self {
            vaddr: mapping.vaddr,
            region: mapping.region,
            provider,
        })
    }

    pub const fn region(&self) -> MmioRegion {
        self.region
    }

    pub fn as_ptr(&self) -> *mut u8 {
        self.vaddr as *mut u8
    }

    #[inline]
    fn access_ptr(&self, offset: usize, size: usize) -> *mut u8 {
        // Alignment is part of the safe contract: `read_volatile::<u32>`
        // requires 4-byte alignment and "registers are naturally aligned"
        // comments cannot constrain a safe `usize` offset. Misalignment
        // panics here instead of being UB at the volatile op.
        #[expect(
            clippy::panic,
            reason = "CG-021-no-panic-on-reachable-failure: a misaligned MMIO offset is a driver \
                      programming error; a loud immediate failure beats UB at the volatile op"
        )]
        {
            let align = size.max(1);
            if !(self.vaddr + offset).is_multiple_of(align) {
                panic!(
                    "MMIO access misaligned: vaddr {:#x} + offset {:#x} not {}-byte aligned",
                    self.vaddr, offset, align
                );
            }
        }
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: offsets come from region-bounded \
                      register accessors; overflow means a malformed accessor, not runtime data"
        )]
        let end = offset
            .checked_add(size)
            .expect("MMIO access offset overflow");
        assert!(end <= self.region.size, "MMIO access out of bounds");
        // SAFETY: the mapping covers `region.size` bytes and the checked
        // offset is within it.
        unsafe { self.as_ptr().add(offset) }
    }

    #[inline]
    pub fn read8(&self, offset: usize) -> u8 {
        // SAFETY: `access_ptr` bounds-checks the offset against the mapped
        // region and the pointer is aligned for u8 by construction.
        let value = unsafe { ptr::read_volatile(self.access_ptr(offset, 1)) };
        fence(Ordering::Acquire);
        value
    }

    #[inline]
    pub fn read16(&self, offset: usize) -> u16 {
        // SAFETY: bounds-checked by `access_ptr`; MMIO u16 registers are
        // naturally aligned, so the cast keeps alignment.
        let value = unsafe { ptr::read_volatile(self.access_ptr(offset, 2) as *const u16) };
        fence(Ordering::Acquire);
        value
    }

    #[inline]
    pub fn read32(&self, offset: usize) -> u32 {
        // SAFETY: bounds-checked by `access_ptr`; MMIO u32 registers are
        // naturally aligned, so the cast keeps alignment.
        let value = unsafe { ptr::read_volatile(self.access_ptr(offset, 4) as *const u32) };
        fence(Ordering::Acquire);
        value
    }

    #[inline]
    pub fn read64(&self, offset: usize) -> u64 {
        // SAFETY: bounds-checked by `access_ptr`; MMIO u64 registers are
        // naturally aligned, so the cast keeps alignment.
        let value = unsafe { ptr::read_volatile(self.access_ptr(offset, 8) as *const u64) };
        fence(Ordering::Acquire);
        value
    }

    #[inline]
    pub fn write8(&self, offset: usize, value: u8) {
        fence(Ordering::Release);
        // SAFETY: bounds-checked by `access_ptr`; u8 needs no alignment.
        unsafe { ptr::write_volatile(self.access_ptr(offset, 1), value) };
    }

    #[inline]
    pub fn write16(&self, offset: usize, value: u16) {
        fence(Ordering::Release);
        // SAFETY: bounds-checked by `access_ptr`; naturally aligned for u16.
        unsafe { ptr::write_volatile(self.access_ptr(offset, 2) as *mut u16, value) };
    }

    #[inline]
    pub fn write32(&self, offset: usize, value: u32) {
        fence(Ordering::Release);
        // SAFETY: bounds-checked by `access_ptr`; naturally aligned for u32.
        unsafe { ptr::write_volatile(self.access_ptr(offset, 4) as *mut u32, value) };
    }

    #[inline]
    pub fn write64(&self, offset: usize, value: u64) {
        fence(Ordering::Release);
        // SAFETY: bounds-checked by `access_ptr`; naturally aligned for u64.
        unsafe { ptr::write_volatile(self.access_ptr(offset, 8) as *mut u64, value) };
    }
}

impl Drop for IoMap {
    fn drop(&mut self) {
        // SAFETY: this non-Clone guard returns its original mapping exactly once.
        unsafe {
            self.provider.unmap_mmio(MmioMapping {
                vaddr: self.vaddr,
                region: self.region,
            });
        }
    }
}

/// A physical MMIO region described by firmware.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MmioRegion {
    pub base: usize,
    pub size: usize,
}

impl MmioRegion {
    pub const fn new(base: usize, size: usize) -> Self {
        Self { base, size }
    }
}

/// An x86 I/O port range described by firmware or PCI BARs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IoPortRange {
    pub base: u16,
    pub size: u16,
}

/// A mapping token returned by [`MmioOp::map_mmio`].
///
/// The provider is responsible for interpreting the token in
/// [`MmioOp::unmap_mmio`]. Copying or constructing this metadata does not create
/// an acquisition. Only the original unreleased token may be passed to unsafe
/// unmap; drivers normally retain an [`IoMap`] instead.
#[derive(Clone, Copy, Debug)]
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
/// # Safety
/// Successful mappings must be non-null and valid for volatile access throughout
/// the returned region until their matching unmap. Concurrent acquisitions must
/// retain independent mapping references. Implementations synchronize their state.
pub unsafe trait MmioOp: Sync {
    /// Map an MMIO region and return a token for later teardown.
    fn map_mmio(&self, region: MmioRegion, name: &'static str) -> ResResult<MmioMapping>;

    /// Release a mapping previously returned by [`Self::map_mmio`].
    ///
    /// # Safety
    /// The token must be an unreleased acquisition from this provider, and all
    /// accesses through that acquisition must have ended.
    unsafe fn unmap_mmio(&self, mapping: MmioMapping);
}

unittest::path_mod_test!(tests, "../ut-tests/mmio.rs");

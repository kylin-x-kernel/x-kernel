// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{
    fmt,
    ops::{Deref, DerefMut},
};

use memaddr::{PhysAddr, VirtAddr};

/// A memory range represented as (start, size).
pub type MemRange = (usize, usize);
/// Legacy alias kept for older platform-local code.
pub type RawRange = MemRange;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReservedKind {
    Firmware,
    Acpi,
    Persistent,
    Unusable,
    BootRuntime,
    Initrd,
    KernelImage,
    DevicePrivate,
    Platform,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReservedSource {
    DeviceTree,
    Acpi,
    BootProtocol,
    Kernel,
    Platform,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReservedRegion {
    pub start: usize,
    pub size: usize,
    pub kind: ReservedKind,
    pub source: ReservedSource,
    pub name: &'static str,
}

impl ReservedRegion {
    pub const EMPTY: Self = Self::new(0, 0, ReservedKind::Platform, ReservedSource::Platform, "");

    pub const fn new(
        start: usize,
        size: usize,
        kind: ReservedKind,
        source: ReservedSource,
        name: &'static str,
    ) -> Self {
        Self {
            start,
            size,
            kind,
            source,
            name,
        }
    }

    pub const fn range(self) -> MemRange {
        (self.start, self.size)
    }

    pub const fn is_empty(self) -> bool {
        self.size == 0
    }
}

#[repr(align(4096))]
/// Wrapper that enforces 4K alignment for static values.
pub struct PageAligned<T: Sized>(T);

impl<T: Sized> PageAligned<T> {
    pub const fn new(v: T) -> Self {
        Self(v)
    }
}

impl<T> Deref for PageAligned<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> DerefMut for PageAligned<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

pub type Aligned4K<T> = PageAligned<T>;

bitflags::bitflags! {
    #[derive(Clone, Copy)]
    /// Memory region attributes.
    pub struct MemFlags: usize {
        const READ = 1 << 0;
        const WRITE = 1 << 1;
        const EXECUTE = 1 << 2;
        const DEVICE = 1 << 4;
        const UNCACHED = 1 << 5;
        const RESERVED = 1 << 6;
        const FREE = 1 << 7;

        const R = 1 << 0;
        const W = 1 << 1;
        const X = 1 << 2;
        const DEV = 1 << 4;
        const UC = 1 << 5;
        const RSVD = 1 << 6;
    }
}

impl fmt::Debug for MemFlags {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

/// Default flags for usable RAM.
pub const RAM_DEF: MemFlags = MemFlags::R.union(MemFlags::W).union(MemFlags::FREE);
/// Default flags for reserved memory.
pub const RSVD_DEF: MemFlags = MemFlags::R.union(MemFlags::W).union(MemFlags::RSVD);
/// Default flags for MMIO regions.
pub const MMIO_DEF: MemFlags = MemFlags::R
    .union(MemFlags::W)
    .union(MemFlags::DEV)
    .union(MemFlags::RSVD);
#[derive(Debug, Clone, Copy)]
pub struct MemoryRegion {
    /// Physical start address.
    pub paddr: PhysAddr,
    /// Optional override for the virtual address of this region.
    pub vaddr: Option<VirtAddr>,
    /// Size in bytes.
    pub size: usize,
    /// Region attributes.
    pub flags: MemFlags,
    /// Human-readable name for diagnostics.
    pub name: &'static str,
}

impl MemoryRegion {
    pub const fn new_ram(s: usize, n: usize, name: &'static str) -> Self {
        Self {
            paddr: PhysAddr::from_usize(s),
            vaddr: None,
            size: n,
            flags: RAM_DEF,
            name,
        }
    }

    pub const fn new_mmio(s: usize, n: usize, name: &'static str) -> Self {
        Self {
            paddr: PhysAddr::from_usize(s),
            vaddr: None,
            size: n,
            flags: MMIO_DEF,
            name,
        }
    }

    pub const fn new_rsvd(s: usize, n: usize, name: &'static str) -> Self {
        Self {
            paddr: PhysAddr::from_usize(s),
            vaddr: None,
            size: n,
            flags: RSVD_DEF,
            name,
        }
    }
}

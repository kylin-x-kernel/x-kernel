// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Minimal guest-address and access-width types for the ported VirtIO layer.
//!
//! The ArceOS sources model guest physical addresses on `axvm_types` /
//! `ax_memory_addr` (`def_usize_addr`); VMMs pass raw guest physical addresses
//! through their MMIO bus and DMA translation, so the port defines the minimal
//! `usize`-backed newtypes the VirtIO queue/MMIO data paths need, mirroring
//! `def_usize_addr` exactly.

/// Guest physical address with the helper methods the VirtIO data paths use.
///
/// Backed by `usize` like `ax_memory_addr`'s `def_usize_addr` types so that
/// untyped integer literals (`GuestPhysAddr::from(0)`) resolve unambiguously.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct GuestPhysAddr(usize);

impl GuestPhysAddr {
    /// Converts a `usize` to a guest physical address.
    pub const fn from_usize(addr: usize) -> Self {
        Self(addr)
    }

    /// Converts a guest physical address to a `usize`.
    pub const fn as_usize(self) -> usize {
        self.0
    }
}

impl From<usize> for GuestPhysAddr {
    fn from(value: usize) -> Self {
        Self(value)
    }
}

impl From<GuestPhysAddr> for usize {
    fn from(value: GuestPhysAddr) -> usize {
        value.0
    }
}

impl core::ops::Add<usize> for GuestPhysAddr {
    type Output = Self;

    fn add(self, rhs: usize) -> Self {
        Self(self.0 + rhs)
    }
}

impl core::ops::Sub<usize> for GuestPhysAddr {
    type Output = Self;

    fn sub(self, rhs: usize) -> Self {
        Self(self.0 - rhs)
    }
}

/// Host physical address as returned by
/// [`GuestMemoryAccessor::translate_and_get_limit`](crate::memory::GuestMemoryAccessor::translate_and_get_limit).
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct PhysAddr(usize);

impl PhysAddr {
    /// Converts a `usize` to a host physical address.
    pub const fn from_usize(addr: usize) -> Self {
        Self(addr)
    }

    /// Converts a host physical address to a `usize`.
    pub const fn as_usize(self) -> usize {
        self.0
    }
}

impl From<usize> for PhysAddr {
    fn from(value: usize) -> Self {
        Self(value)
    }
}

/// MMIO bus access width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessWidth {
    /// 8-bit access.
    Byte,
    /// 16-bit access.
    Word,
    /// 32-bit access.
    Dword,
    /// 64-bit access.
    Qword,
}

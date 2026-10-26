// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! MM-owned mapping-attribute capabilities, independent of allocation and device ownership.

#![no_std]

use memaddr::VirtAddr;

/// The two kernel mapping attribute sets used by coherent DMA allocations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DmaMappingMode {
    /// Read/write, uncached and platform-shared DMA pages.
    Coherent,
    /// Ordinary read/write kernel pages before returning them to the allocator.
    Normal,
}

/// MM-owned mapping-attribute transition, not DMA allocation or platform sharing.
#[kiface::interface]
pub trait DmaMappingIf {
    /// Applies one DMA mapping mode to an existing kernel virtual range.
    ///
    /// Requires initialized kernel address-space management and an exclusively
    /// owned, page-aligned allocation. The caller quiesces device access before
    /// restoring Normal and retains ownership until this operation completes.
    /// This does not allocate pages, share them with firmware or synchronize
    /// device DMA. The provider owns page-table locking and flag selection.
    ///
    /// # Errors
    /// Propagates range validation and page-table protection failures.
    fn set_mode(vaddr: VirtAddr, size_bytes: usize, mode: DmaMappingMode) -> kerrno::KResult;
}

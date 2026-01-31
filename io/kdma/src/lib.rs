// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! DMA allocation helpers and bus address types.
#![no_std]

extern crate alloc;

mod dma;

use core::{alloc::Layout, ptr::NonNull};

use alloc_engine::AllocResult;
// Re-export the interface trait for implementors
pub use dma::DmaPageTableIf;
use memaddr::PhysAddr;

use self::dma::ALLOCATOR;

/// Converts a physical address to a bus address.
///
/// It assumes that there is a linear mapping with the offset
/// [`platconfig::plat::PHYS_BUS_OFFSET`], that maps all the physical memory
/// to the virtual space at the address plus the offset. So we have
/// `baddr = paddr + PHYS_BUS_OFFSET`.
#[inline]
pub const fn p2b(paddr: PhysAddr) -> DmaBusAddress {
    DmaBusAddress::new((paddr.as_usize() + platconfig::plat::PHYS_BUS_OFFSET) as u64)
}

/// Allocates **coherent** memory that meets Direct Memory Access (DMA)
/// requirements.
///
/// This function allocates a block of memory through the global allocator. The
/// memory pages must be contiguous, undivided, and have consistent read and
/// write access.
///
/// - `layout`: The memory layout, which describes the size and alignment
///   requirements of the requested memory.
///
/// Returns an [`DMAInfo`] structure containing details about the allocated
/// memory, such as the starting address and size. If it's not possible to
/// allocate memory meeting the criteria, returns [`None`].
///
/// # Safety
///
/// This function is unsafe because it directly interacts with the global
/// allocator, which can potentially cause memory leaks or other issues if not
/// used correctly.
pub unsafe fn allocate_dma_memory(layout: Layout) -> AllocResult<DMAInfo> {
    unsafe { ALLOCATOR.lock().allocate_dma_memory(layout) }
}

/// Frees coherent memory previously allocated.
///
/// This function releases the memory block that was previously allocated and
/// marked as coherent. It ensures proper deallocation and management of resources
/// associated with the memory block.
///
/// - `dma_info`: An instance of [`DMAInfo`] containing the details of the memory
///   block to be freed, such as its starting address and size.
///
/// # Safety
///
/// This function is unsafe because it directly interacts with the global allocator,
/// which can potentially cause memory leaks or other issues if not used correctly.
pub unsafe fn deallocate_dma_memory(dma: DMAInfo, layout: Layout) {
    unsafe { ALLOCATOR.lock().deallocate_dma_memory(dma, layout) }
}

/// A bus memory address.
///
/// It's a wrapper type around an [`u64`].
#[repr(transparent)]
#[derive(Copy, Clone, Default, Ord, PartialOrd, Eq, PartialEq)]
pub struct DmaBusAddress(u64);

impl DmaBusAddress {
    /// Converts an [`u64`] to a physical address.
    pub const fn new(addr: u64) -> Self {
        Self(addr)
    }

    /// Converts the address to an [`u64`].
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

impl From<u64> for DmaBusAddress {
    fn from(value: u64) -> Self {
        Self::new(value)
    }
}

impl core::fmt::Debug for DmaBusAddress {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("DmaBusAddress")
            .field(&format_args!("{:#X}", self.0))
            .finish()
    }
}

/// Represents information related to a DMA operation.
#[derive(Debug, Clone, Copy)]
pub struct DMAInfo {
    /// The address at which the CPU accesses this memory region. This address
    /// is a virtual memory address used by the CPU to access memory.
    pub cpu_addr: NonNull<u8>,
    /// Represents the physical address of this memory region on the bus. The DMA
    /// controller uses this address to directly access memory.
    pub bus_addr: DmaBusAddress,
}

// NOTICE: Portions of this file may appear structurally similar to code in other open source projects (e.g., libsql/libsql, optuna/kurobako, novifinancial/winterfell),
// but the semantics and implementation intent are entirely different. Any resemblance is coincidental and does not indicate code reuse or derivation.
//
// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! DMA allocation helpers and bus address types.
//!
//! `kdma` is the kernel's DMA memory layer: [`allocate_dma_memory`] and
//! [`deallocate_dma_memory`] manage coherent, device-reachable memory;
//! [`map_dma_buffer`] / [`unmap_dma_buffer`] stage ordinary buffers
//! through a coherent bounce pool for temporary transactions; [`p2b`] /
//! [`b2p`] convert between physical and bus addresses. See
//! `docs/design.md` for the full boundary contracts.
//!
//! # Example
//!
//! Map a driver-owned buffer for a device read (DeviceToDriver), then
//! unmap it once the device is done. Kernel code only; this crate cannot
//! compile in a host doctest environment, so the example is `ignore`.
//!
//! ```ignore
//! use core::ptr::NonNull;
//! use kdma::{map_dma_buffer, unmap_dma_buffer, DmaDirection};
//!
//! # fn stage(buffer: NonNull<[u8]>) -> Result<(), alloc_engine::AllocError> {
//! // 1. Stage the driver-owned buffer through the coherent bounce pool.
//! let dma = unsafe { map_dma_buffer(buffer, DmaDirection::DeviceToDriver) }?;
//!
//! // 2. Program the device with dma.bus_addr and wait for completion.
//! // device.submit(dma.bus_addr, buffer.len());
//!
//! // 3. Unmap exactly once with the same address and buffer.
//! unsafe { unmap_dma_buffer(dma.bus_addr, buffer, DmaDirection::DeviceToDriver) };
//! # Ok(())
//! # }
//! ```
//!
//! For persistent device-reachable memory (descriptor rings, shared
//! queues), use [`allocate_dma_memory`] instead, which returns a
//! [`DmaInfo`] whose bus address stays valid until
//! [`deallocate_dma_memory`].
#![no_std]

extern crate alloc;

mod bounce_pool;
mod dma;

use core::{alloc::Layout, ptr::NonNull};

use alloc_engine::AllocResult;
pub use dma::DmaPageTableIf;
use memaddr::PhysAddr;

use self::dma::ALLOCATOR;

/// Converts a physical address to a bus address.
///
/// It assumes that there is a linear mapping with the offset
/// [`kbuild_config::PHYS_BUS_OFFSET`], that maps all the physical memory
/// to the virtual space at the address plus the offset. So we have
/// `baddr = paddr + PHYS_BUS_OFFSET`.
#[inline]
pub const fn p2b(paddr: PhysAddr) -> DmaBusAddress {
    DmaBusAddress::new((paddr.as_usize() + kbuild_config::PHYS_BUS_OFFSET) as u64)
}

/// Converts a bus address back to a physical address.
#[inline]
pub fn b2p(baddr: DmaBusAddress) -> PhysAddr {
    ((baddr.as_u64() as usize) - kbuild_config::PHYS_BUS_OFFSET).into()
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
/// Returns an [`DmaInfo`] structure containing details about the allocated
/// memory, such as the starting address and size. If it's not possible to
/// allocate memory meeting the criteria, returns [`None`].
///
/// # Errors
///
/// Returns `AllocError::NoMemory` when pages cannot be allocated, the
/// mapping-attribute update fails, or the platform `prepare` hook fails.
///
/// # Safety
///
/// The caller must free the returned allocation exactly once with
/// [`deallocate_dma_memory`] using the same `layout`, and must only expose the
/// returned bus address to devices that are allowed to DMA that buffer.
#[track_caller]
pub unsafe fn allocate_dma_memory(layout: Layout) -> AllocResult<DmaInfo> {
    // SAFETY: forwards the caller's DMA allocation preconditions to the global DMA allocator.
    unsafe { ALLOCATOR.lock().allocate_dma_memory(layout) }
}

/// Frees coherent memory previously allocated.
///
/// This function releases the memory block that was previously allocated and
/// marked as coherent. It ensures proper deallocation and management of resources
/// associated with the memory block.
///
/// - `dma_info`: An instance of [`DmaInfo`] containing the details of the memory
///   block to be freed, such as its starting address and size.
///
/// # Safety
///
/// `dma` must come from a prior successful [`allocate_dma_memory`] call, and
/// `layout` must exactly match that allocation.
#[track_caller]
pub unsafe fn deallocate_dma_memory(dma: DmaInfo, layout: Layout) {
    // SAFETY: forwards the caller's matching deallocation contract to the global DMA allocator.
    unsafe { ALLOCATOR.lock().deallocate_dma_memory(dma, layout) }
}

/// Direction for a temporary DMA mapping of an existing buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmaDirection {
    /// The device reads data the CPU produced (e.g. a TX descriptor ring).
    DriverToDevice,
    /// The device produces data the CPU reads (e.g. an RX descriptor ring).
    DeviceToDriver,
    /// The buffer is both read and written by the device.
    Bidirectional,
}

/// Maps an existing buffer for temporary DMA access.
///
/// The current backend always stages the mapping through pooled coherent bounce
/// memory. Keeping this behind `kdma` lets future implementations restore
/// direct-map optimizations with stricter page-granularity safety checks
/// without changing driver code.
///
/// # Errors
///
/// Returns `AllocError::InvalidInput` for a zero-length buffer or a bus
/// address that is already actively mapped, and
/// `AllocError::NoMemory` when the bounce pool cannot satisfy the
/// request.
///
/// # Safety
///
/// `buffer` must remain live and exclusively owned until it is later passed to
/// [`unmap_dma_buffer`]. The caller must not let the device keep using the
/// returned bus address after unmapping.
pub unsafe fn map_dma_buffer(
    buffer: NonNull<[u8]>,
    direction: DmaDirection,
) -> AllocResult<DmaInfo> {
    // SAFETY: forwards the caller's exclusive-buffer contract to the DMA mapper.
    unsafe { ALLOCATOR.lock().map_dma_buffer(buffer, direction) }
}

/// Unmaps a previously mapped DMA buffer and synchronizes data back if needed.
///
/// # Safety
///
/// `dma_addr` must be the still-active bus address previously returned by
/// [`map_dma_buffer`] for the same `buffer`, and unmapping must happen exactly
/// once for that mapping.
/// # Panics
///
/// Panics when `dma_addr` is not an active mapping or when `buffer`'s
/// length does not match the length recorded at [`map_dma_buffer`] time;
/// both are caller contract violations, not runtime conditions.
///
/// # Example
///
/// The unmap must repeat the exact address, buffer, and length from the
/// matching [`map_dma_buffer`] call — see the crate-level
/// [example](crate#example) for the full map → device → unmap sequence.
pub unsafe fn unmap_dma_buffer(
    dma_addr: DmaBusAddress,
    buffer: NonNull<[u8]>,
    direction: DmaDirection,
) {
    // SAFETY: forwards the caller's matching mapping contract to the DMA mapper.
    unsafe {
        ALLOCATOR
            .lock()
            .unmap_dma_buffer(dma_addr, buffer, direction)
    }
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
pub struct DmaInfo {
    /// The address at which the CPU accesses this memory region. This address
    /// is a virtual memory address used by the CPU to access memory.
    pub cpu_addr: NonNull<u8>,
    /// Represents the physical address of this memory region on the bus. The DMA
    /// controller uses this address to directly access memory.
    pub bus_addr: DmaBusAddress,
}

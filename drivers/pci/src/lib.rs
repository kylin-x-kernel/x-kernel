// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Structures and functions for PCI bus operations.
//!
//! Currently, it just re-exports structures from the crate [virtio-drivers][1]
//! and its module [`virtio_drivers::transport::pci::bus`][2].
//!
//! [1]: https://docs.rs/virtio-drivers/latest/virtio_drivers/
//! [2]: https://docs.rs/virtio-drivers/latest/virtio_drivers/transport/pci/bus/index.html

#![no_std]

pub use virtio_drivers::transport::pci::bus::{
    BarInfo, Cam, CapabilityInfo, Command, ConfigurationAccess, DeviceFunction, DeviceFunctionInfo,
    HeaderType, MemoryBarType, MmioCam, PciError, PciRoot, Status,
};

pub mod msix;

/// Provides read/write access to PCI configuration space registers.
///
/// The `virtio-drivers` crate exposes `PciRoot` but keeps its internal
/// `config_read_word` / `config_write_word` methods `pub(crate)`. This type
/// re-implements the same memory-mapped access so that callers outside of
/// `virtio-drivers` (e.g. the MSI-X subsystem) can also read/write arbitrary
/// configuration space registers.
///
/// Create one from the same MMIO base pointer and [`Cam`] type used for
/// [`PciRoot`]. Both types reference the same underlying memory region.
pub struct PciConfigAccess {
    mmio_base: *mut u32,
    cam: Cam,
}

// SAFETY: PciConfigAccess is used like PciRoot: the raw pointer points to a
// static MMIO mapping that is valid for the lifetime of the program.
unsafe impl Send for PciConfigAccess {}
unsafe impl Sync for PciConfigAccess {}

impl PciConfigAccess {
    /// Creates a new `PciConfigAccess` from a raw MMIO base and CAM type.
    ///
    /// # Safety
    ///
    /// `mmio_base` must meet the same requirements as for [`PciRoot::new`]:
    /// it must be a valid, 4-byte-aligned, appropriately-mapped MMIO pointer
    /// valid for the lifetime of the program.
    pub unsafe fn new(mmio_base: *mut u8, cam: Cam) -> Self {
        assert!(mmio_base as usize & 0x3 == 0);
        Self {
            mmio_base: mmio_base as *mut u32,
            cam,
        }
    }

    /// Computes the byte offset of a register within the flat MMIO CAM window.
    fn cam_offset(&self, device_function: DeviceFunction, register_offset: u16) -> u32 {
        let bdf = (device_function.bus as u32) << 8
            | (device_function.device as u32) << 3
            | device_function.function as u32;
        let shift = match self.cam {
            Cam::MmioCam => 8,
            Cam::Ecam => 12,
        };
        (bdf << shift) | (register_offset as u32 & !0x3)
    }

    /// Reads a 32-bit word from PCI configuration space.
    ///
    /// `register_offset` is the byte offset of the register; the two LSBs are
    /// ignored (the access is always 32-bit aligned).
    pub fn read_word(&self, device_function: DeviceFunction, register_offset: u16) -> u32 {
        let address = self.cam_offset(device_function, register_offset);
        // SAFETY: The pointer arithmetic stays within the MMIO window because
        // cam_offset() produces offsets bounded by Cam::size().
        unsafe { self.mmio_base.add((address >> 2) as usize).read_volatile() }
    }

    /// Writes a 32-bit word to PCI configuration space.
    ///
    /// `register_offset` is the byte offset; the two LSBs are ignored.
    pub fn write_word(&mut self, device_function: DeviceFunction, register_offset: u16, data: u32) {
        let address = self.cam_offset(device_function, register_offset);
        // SAFETY: Same as read_word.
        unsafe {
            self.mmio_base
                .add((address >> 2) as usize)
                .write_volatile(data);
        }
    }
}

/// Sequential allocator for MMIO address space used by PCI Base Address Registers.
///
/// This allocator manages a contiguous range of physical addresses and dispenses
/// aligned chunks suitable for memory-mapped I/O regions. Each allocation must
/// be power-of-two sized and will be naturally aligned to that size.
pub struct PciRangeAllocator {
    _begin_addr: u64,
    end_addr: u64,
    cursor_addr: u64,
}

impl PciRangeAllocator {
    /// Constructs a new allocator managing the specified address range.
    ///
    /// # Arguments
    ///
    /// * `start` - The starting physical address of the MMIO region
    /// * `length` - The total size in bytes of the allocatable region
    pub const fn new(start: u64, length: u64) -> Self {
        Self {
            _begin_addr: start,
            end_addr: start.saturating_add(length),
            cursor_addr: start,
        }
    }

    /// Attempts to allocate an aligned MMIO region of the requested size.
    ///
    /// # Arguments
    ///
    /// * `length` - The requested allocation size. Must be a power of two.
    ///
    /// # Returns
    ///
    /// * `Some(address)` - The base address of the allocated region, aligned to `length`
    /// * `None` - If `length` is not a power of two or insufficient space remains
    ///
    /// # Alignment guarantee
    ///
    /// The returned address is guaranteed to be a multiple of `length`, ensuring
    /// natural alignment for the allocated region.
    pub fn alloc_buf(&mut self, length: u64) -> Option<u64> {
        // Validate that length is a power of two (required for alignment)
        if length == 0 || (length & (length.wrapping_sub(1))) != 0 {
            return None;
        }

        // Calculate the aligned starting address for this allocation
        let aligned_addr = self.compute_aligned_address(length)?;

        // Verify that the full allocation fits within our managed range
        let allocation_end = aligned_addr.checked_add(length)?;
        if allocation_end > self.end_addr {
            return None;
        }

        // Commit the allocation by advancing our free pointer
        self.cursor_addr = allocation_end;
        Some(aligned_addr)
    }

    /// Computes the next address aligned to the given boundary.
    ///
    /// Returns `None` if alignment calculation would overflow.
    fn compute_aligned_address(&self, alignment: u64) -> Option<u64> {
        // Calculate the alignment mask (e.g., for alignment=4096, mask=4095)
        let alignment_mask = alignment.wrapping_sub(1);

        // Apply ceiling alignment: (addr + mask) & ~mask
        let adjusted = self.cursor_addr.checked_add(alignment_mask)?;
        Some(adjusted & !alignment_mask)
    }
}

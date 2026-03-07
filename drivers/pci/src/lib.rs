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
    BarInfo, Cam, CapabilityInfo, Command, DeviceFunction, DeviceFunctionInfo, HeaderType,
    MemoryBarType, PciError, PciRoot, Status,
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

/// Used to allocate MMIO regions for PCI BARs.
pub struct PciRangeAllocator {
    _start: u64,
    end: u64,
    current: u64,
}

impl PciRangeAllocator {
    /// Creates a new allocator from a memory range.
    pub const fn new(base: u64, size: u64) -> Self {
        Self {
            _start: base,
            end: base + size,
            current: base,
        }
    }

    /// Allocates a memory region with the given size.
    ///
    /// The `size` should be a power of 2, and the returned value is also a
    /// multiple of `size`.
    pub fn alloc_buf(&mut self, size: u64) -> Option<u64> {
        if !size.is_power_of_two() {
            return None;
        }
        let ret = align_up(self.current, size);
        if ret + size > self.end {
            return None;
        }

        self.current = ret + size;
        Some(ret)
    }
}

const fn align_up(addr: u64, align: u64) -> u64 {
    (addr + align - 1) & !(align - 1)
}

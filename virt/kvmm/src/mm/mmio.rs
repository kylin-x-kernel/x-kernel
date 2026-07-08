// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! MMIO device bus for guest I/O emulation.
//!
//! When a guest accesses an address that is mapped as device memory,
//! the second-stage page table triggers a fault (Data Abort on AArch64,
//! EPT Violation on x86_64, Guest Page Fault on RISC-V). The exit
//! handler extracts the guest physical address and dispatches it to
//! the [`MmioBus`], which routes it to the appropriate [`MmioDevice`].

use alloc::{boxed::Box, vec::Vec};

/// A single MMIO device that handles reads and writes at a fixed
/// address range in guest physical address space.
pub trait MmioDevice: Send {
    /// Returns the `(base_gpa, size)` of this device's MMIO region.
    fn mmio_range(&self) -> (u64, u64);

    /// Handle an MMIO read. Returns the value to inject into the guest.
    fn read(&self, offset: u64, size: u8) -> u64;

    /// Handle an MMIO write from the guest.
    fn write(&mut self, offset: u64, size: u8, value: u64);
}

/// MMIO bus that dispatches guest memory-mapped I/O to registered devices.
pub struct MmioBus {
    devices: Vec<Box<dyn MmioDevice>>,
}

impl Default for MmioBus {
    fn default() -> Self {
        Self::new()
    }
}

impl MmioBus {
    pub fn new() -> Self {
        Self {
            devices: Vec::new(),
        }
    }

    pub fn register(&mut self, dev: Box<dyn MmioDevice>) {
        self.devices.push(dev);
    }

    /// Dispatch an MMIO access to the matching device.
    ///
    /// For reads (`is_write=false`), returns `Some(value)`.
    /// For writes (`is_write=true`), returns `Some(0)` on success.
    /// Returns `None` if no device handles this address.
    pub fn handle(&mut self, gpa: u64, is_write: bool, size: u8, value: u64) -> Option<u64> {
        for dev in &mut self.devices {
            let (base, dev_size) = dev.mmio_range();
            if gpa >= base && gpa < base + dev_size {
                let offset = gpa - base;
                return if is_write {
                    dev.write(offset, size, value);
                    Some(0)
                } else {
                    Some(dev.read(offset, size))
                };
            }
        }
        None
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! CPU controller replacing only the pinned SDK import in the real adapter.

use core::marker::PhantomData;

pub trait Hal {
    fn virt_to_phys(address: usize) -> usize;
    fn current_ms() -> u64;
    fn flush_dcache();
}

pub(crate) struct AhciDriver<H> {
    pub(crate) storage: [u8; 4096],
    pub(crate) reads: usize,
    pub(crate) writes: usize,
    pub(crate) last_read_address: usize,
    pub(crate) last_write_address: usize,
    pub(crate) fail_read: bool,
    pub(crate) fail_write: bool,
    // The fixture invokes only static HAL functions and owns no H value.
    marker: PhantomData<fn() -> H>,
}

impl<H: Hal> AhciDriver<H> {
    /// Creates the CPU controller; it performs no MMIO or hardware DMA.
    ///
    /// # Safety
    /// The adapter must retain its borrowed MMIO backing for this call, as it
    /// does for the real SDK. The fixture only inspects its address via the HAL.
    pub(crate) unsafe fn try_new(address: usize) -> Option<Self> {
        // Non-coherent test HALs panic here if the adapter fails to reject them.
        let _physical = H::virt_to_phys(address);
        let _time = H::current_ms();
        H::flush_dcache();
        Some(Self {
            storage: [0x73; 4096],
            reads: 0,
            writes: 0,
            last_read_address: 0,
            last_write_address: 0,
            fail_read: false,
            fail_write: false,
            marker: PhantomData,
        })
    }

    pub(crate) fn block_size(&self) -> usize {
        512
    }

    pub(crate) fn capacity(&self) -> u64 {
        8
    }

    pub(crate) fn read(&mut self, block: u64, bytes: &mut [u8]) -> bool {
        self.reads += 1;
        self.last_read_address = bytes.as_ptr() as usize;
        if self.fail_read {
            bytes[..17].fill(0xee);
            return false;
        }
        let start = block as usize * 512;
        bytes.copy_from_slice(&self.storage[start..start + bytes.len()]);
        true
    }

    pub(crate) fn write(&mut self, block: u64, bytes: &[u8]) -> bool {
        self.writes += 1;
        self.last_write_address = bytes.as_ptr() as usize;
        if self.fail_write {
            return false;
        }
        let start = block as usize * 512;
        self.storage[start..start + bytes.len()].copy_from_slice(bytes);
        true
    }
}

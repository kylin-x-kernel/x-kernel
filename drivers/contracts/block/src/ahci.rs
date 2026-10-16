// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use driver_base::{Device, DeviceKind, DriverError, DriverResult};
use kspin::SpinNoIrq;
use simple_ahci::AhciDriver as CoreAhciDriver;
pub use simple_ahci::Hal as AhciHal;

use crate::BlockDeviceOperations;

/// AHCI driver implementation based on `simple_ahci` crate.
///
/// The `SpinNoIrq` lock serializes hardware access so the device can be
/// shared behind a `&self` interface. `num_blocks` and `block_size` cache
/// the device geometry so queries do not need to take the lock.
pub struct AhciDriver<H: AhciHal> {
    inner: SpinNoIrq<CoreAhciDriver<H>>,
    num_blocks: u64,
    block_size: usize,
}

impl<H: AhciHal> AhciDriver<H> {
    /// Attempts to create a new AHCI driver using the specified MMIO base address.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - `base_addr` refers to a valid MMIO register block of the AHCI controller.
    /// - The memory region from `base_addr` onward is mapped and accessible.
    /// - No other part of the code is accessing the AHCI controller simultaneously.
    /// - The AHCI hardware is functioning at the provided address.
    pub unsafe fn new(base_addr: usize) -> Option<Self> {
        // SAFETY: the caller upholds the `# Safety` contract of this function:
        // `base_addr` is the mapped MMIO register block of a present, working
        // AHCI controller that no other code accesses concurrently, which is
        // exactly the contract `simple_ahci::AhciDriver::try_new` requires.
        unsafe { CoreAhciDriver::<H>::try_new(base_addr) }.map(|driver| {
            let block_size = driver.block_size();
            let num_blocks = driver.capacity();
            AhciDriver {
                inner: SpinNoIrq::new(driver),
                num_blocks,
                block_size,
            }
        })
    }
}

impl<H: AhciHal> Device for AhciDriver<H> {
    fn name(&self) -> &str {
        "ahci"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Block
    }
}

impl<H: AhciHal> BlockDeviceOperations for AhciDriver<H> {
    fn block_size(&self) -> usize {
        self.block_size
    }

    fn num_blocks(&self) -> u64 {
        self.num_blocks
    }

    fn read_block(&self, block_id: u64, buf: &mut [u8]) -> DriverResult {
        if buf.len() % self.block_size != 0 {
            return Err(DriverError::InvalidInput);
        }
        if buf.as_ptr() as usize % 4 != 0 {
            return Err(DriverError::InvalidInput);
        }
        let mut dev = self.inner.lock();
        if dev.read(block_id, buf) {
            Ok(())
        } else {
            Err(DriverError::Io)
        }
    }

    fn write_block(&self, block_id: u64, buf: &[u8]) -> DriverResult {
        if buf.len() % self.block_size != 0 {
            return Err(DriverError::InvalidInput);
        }
        if buf.as_ptr() as usize % 4 != 0 {
            return Err(DriverError::InvalidInput);
        }
        let mut dev = self.inner.lock();
        if dev.write(block_id, buf) {
            Ok(())
        } else {
            Err(DriverError::Io)
        }
    }

    fn flush(&self) -> DriverResult {
        Ok(())
    }
}

unittest::path_mod_test!(pub mod tests_ahci, "../ut-tests/ahci.rs");

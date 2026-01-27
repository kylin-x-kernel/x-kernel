//! AHCI driver.

use driver_base::{DeviceKind, DriverError, DriverOps, DriverResult};
use simple_ahci::AhciDriver as SimpleAhciDriver;
pub use simple_ahci::Hal as AhciHal;

use crate::BlockDriverOps;

/// AHCI driver based on the `simple_ahci` crate.
pub struct AhciDriver<H: AhciHal>(SimpleAhciDriver<H>);

impl<H: AhciHal> AhciDriver<H> {
    /// Try to construct a new AHCI driver from the given MMIO base address.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - `base` is a valid virtual address pointing to the AHCI controller's MMIO register block.
    /// - The memory region starting at `base` is properly mapped and accessible.
    /// - No other code is concurrently accessing the same AHCI controller.
    /// - The AHCI controller hardware is present and functional at the given address.
    pub unsafe fn try_new(base: usize) -> Option<Self> {
        unsafe { SimpleAhciDriver::<H>::try_new(base) }.map(AhciDriver)
    }
}

impl<H: AhciHal> DriverOps for AhciDriver<H> {
    fn name(&self) -> &str {
        "ahci"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Block
    }
}

impl<H: AhciHal> BlockDriverOps for AhciDriver<H> {
    fn block_size(&self) -> usize {
        self.0.block_size()
    }

    fn num_blocks(&self) -> u64 {
        self.0.capacity()
    }

    fn read_block(&mut self, block_id: u64, buf: &mut [u8]) -> DriverResult {
        if buf.len() % self.block_size() != 0 {
            return Err(DriverError::InvalidInput);
        }
        if buf.as_ptr() as usize % 4 != 0 {
            return Err(DriverError::InvalidInput);
        }
        if self.0.read(block_id, buf) {
            Ok(())
        } else {
            Err(DriverError::Io)
        }
    }

    fn write_block(&mut self, block_id: u64, buf: &[u8]) -> DriverResult {
        if buf.len() % self.block_size() != 0 {
            return Err(DriverError::InvalidInput);
        }
        if buf.as_ptr() as usize % 4 != 0 {
            return Err(DriverError::InvalidInput);
        }
        if self.0.write(block_id, buf) {
            Ok(())
        } else {
            Err(DriverError::Io)
        }
    }

    fn flush(&mut self) -> DriverResult {
        Ok(())
    }
}

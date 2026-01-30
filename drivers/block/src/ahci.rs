#![allow(unsafe_op_in_unsafe_fn)]

use driver_base::{DeviceKind, DriverError, DriverOps, DriverResult};
use simple_ahci::AhciDriver as CoreAhciDriver;
pub use simple_ahci::Hal as AhciHal;

use crate::BlockDriverOps;

/// AHCI driver implementation based on `simple_ahci` crate.
pub struct AhciDriver<H: AhciHal>(CoreAhciDriver<H>);

impl<H: AhciHal> AhciDriver<H> {
    /// Attempts to create a new AHCI driver using the specified MMIO base address.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - `base` refers to a valid MMIO register block of the AHCI controller.
    /// - The memory region from `base` onward is mapped and accessible.
    /// - No other part of the code is accessing the AHCI controller simultaneously.
    /// - The AHCI hardware is functioning at the provided address.
    pub unsafe fn new(base_addr: usize) -> Option<Self> {
        CoreAhciDriver::<H>::try_new(base_addr).map(AhciDriver)
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

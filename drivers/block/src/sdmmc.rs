//! SD/MMC driver based on SDIO.

#![allow(unsafe_op_in_unsafe_fn)]

use driver_base::{DeviceKind, DriverError, DriverOps, DriverResult};
use simple_sdmmc::SdMmc;

use crate::BlockDriverOps;

/// A SD/MMC driver.
pub struct SdMmcDriver(SdMmc);

impl SdMmcDriver {
    /// Creates a new [`SdMmcDriver`] from the given base address.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `base` is a valid pointer to the SD/MMC controller's
    /// register block and that no other code is concurrently accessing the same hardware.
    pub unsafe fn new(base: usize) -> Self {
        Self(SdMmc::new(base))
    }
}

impl DriverOps for SdMmcDriver {
    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Block
    }

    fn name(&self) -> &str {
        "sdmmc"
    }
}

impl BlockDriverOps for SdMmcDriver {
    fn num_blocks(&self) -> u64 {
        self.0.num_blocks()
    }

    fn block_size(&self) -> usize {
        SdMmc::BLOCK_SIZE
    }

    fn read_block(&mut self, block_id: u64, buf: &mut [u8]) -> DriverResult {
        let (blocks, remainder) = buf.as_chunks_mut::<{ SdMmc::BLOCK_SIZE }>();

        if !remainder.is_empty() {
            return Err(DriverError::InvalidInput);
        }

        for (i, block) in blocks.iter_mut().enumerate() {
            self.0.read_block(block_id as u32 + i as u32, block);
        }

        Ok(())
    }

    fn write_block(&mut self, block_id: u64, buf: &[u8]) -> DriverResult {
        let (blocks, remainder) = buf.as_chunks::<{ SdMmc::BLOCK_SIZE }>();

        if !remainder.is_empty() {
            return Err(DriverError::InvalidInput);
        }

        for (i, block) in blocks.iter().enumerate() {
            self.0.write_block(block_id as u32 + i as u32, block);
        }

        Ok(())
    }

    fn flush(&mut self) -> DriverResult {
        Ok(())
    }
}

use axdriver_block::BlockDriverOps;
use driver_base::{DeviceKind, DriverOps, DriverResult};
use virtio_drivers::{Hal, device::blk::VirtIOBlk as InnerDev, transport::Transport};

use crate::as_driver_error;

/// The VirtIO block device driver.
pub struct VirtIoBlkDev<H: Hal, T: Transport> {
    inner: InnerDev<H, T>,
}

unsafe impl<H: Hal, T: Transport> Send for VirtIoBlkDev<H, T> {}
unsafe impl<H: Hal, T: Transport> Sync for VirtIoBlkDev<H, T> {}

impl<H: Hal, T: Transport> VirtIoBlkDev<H, T> {
    /// Creates a new driver instance and initializes the device, or returns
    /// an error if any step fails.
    pub fn try_new(transport: T) -> DriverResult<Self> {
        Ok(Self {
            inner: InnerDev::new(transport).map_err(as_driver_error)?,
        })
    }
}

impl<H: Hal, T: Transport> DriverOps for VirtIoBlkDev<H, T> {
    fn name(&self) -> &str {
        "virtio-blk"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Block
    }
}

impl<H: Hal, T: Transport> BlockDriverOps for VirtIoBlkDev<H, T> {
    #[inline]
    fn num_blocks(&self) -> u64 {
        self.inner.capacity()
    }

    #[inline]
    fn block_size(&self) -> usize {
        virtio_drivers::device::blk::SECTOR_SIZE
    }

    fn read_block(&mut self, block_id: u64, buf: &mut [u8]) -> DriverResult {
        self.inner
            .read_blocks(block_id as _, buf)
            .map_err(as_driver_error)
    }

    fn write_block(&mut self, block_id: u64, buf: &[u8]) -> DriverResult {
        self.inner
            .write_blocks(block_id as _, buf)
            .map_err(as_driver_error)
    }

    fn flush(&mut self) -> DriverResult {
        Ok(())
    }
}

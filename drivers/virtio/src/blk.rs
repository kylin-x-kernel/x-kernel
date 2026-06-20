// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VirtIO block driver adapter.
use block::BlockDevice;
use driver_base::{Device, DeviceKind, DriverResult};
use kspin::SpinNoIrq;
use virtio_drivers::{
    Hal,
    device::blk::{SECTOR_SIZE, VirtIOBlk as InnerDev},
    transport::Transport,
};

use crate::as_driver_error;

/// The VirtIO block device driver.
///
/// Wraps `VirtIOBlk` from `virtio-drivers` and implements the
/// [`BlockDevice`] trait, providing sector-level read/write access to a
/// virtual block device.
///
/// # Type Parameters
///
/// - `H` - VirtIO HAL implementation for DMA allocation.
/// - `T` - Transport layer (MMIO or PCI).
///
/// # Example
///
/// ```ignore
/// let (kind, transport) = virtio::probe_pci_device::<HalImpl, _>(...).unwrap();
/// let mut blk = VirtIoBlkDev::<HalImpl, _>::try_new(transport)?;
/// let mut buf = [0u8; 512];
/// blk.read_block(0, &mut buf)?;
/// ```
pub struct VirtIoBlkDev<H: Hal, T: Transport> {
    device: SpinNoIrq<InnerDev<H, T>>,
    sector_size: usize,
    num_blocks: u64,
}

// SAFETY: VirtIoBlkDev serializes all access to the inner VirtIOBlk through
// its own `SpinNoIrq` lock. The inner VirtIOBlk is not auto Send due to
// PhantomData, but it is safe to transfer across threads behind that lock.
// The lock must mask interrupts for the duration of each device access,
// because the synchronous request path busy-polls the used ring while holding
// the lock. On targets without per-device MSI-X (e.g. x86, where virtio falls
// back to a shared level-triggered INTx line that the block driver never
// acks), leaving local interrupts enabled during the poll lets the shared line
// re-assert continuously and livelock the CPU in an interrupt storm.
unsafe impl<H: Hal, T: Transport> Send for VirtIoBlkDev<H, T> {}
// SAFETY: shared access to the device is serialized by the IRQ-safe lock
// described above, so immutable references may be shared across threads safely.
unsafe impl<H: Hal, T: Transport> Sync for VirtIoBlkDev<H, T> {}

impl<H: Hal, T: Transport> VirtIoBlkDev<H, T> {
    /// Creates a new driver instance and initializes the device, or returns
    /// an error if any step fails.
    ///
    /// # Errors
    ///
    /// Returns `DriverError` if the device fails to initialize (e.g. feature
    /// negotiation failure, queue allocation failure, DMA error).
    pub fn try_new(transport: T) -> DriverResult<Self> {
        let device = Self::init_device(transport)?;
        let num_blocks = device.capacity();
        Ok(Self {
            device: SpinNoIrq::new(device),
            sector_size: SECTOR_SIZE,
            num_blocks,
        })
    }

    fn init_device(transport: T) -> DriverResult<InnerDev<H, T>> {
        InnerDev::new(transport).map_err(as_driver_error)
    }

    fn read_sector(&self, sector: u64, out_buf: &mut [u8]) -> DriverResult {
        self.device
            .lock()
            .read_blocks(sector as usize, out_buf)
            .map_err(as_driver_error)
    }

    fn write_sector(&self, sector: u64, in_buf: &[u8]) -> DriverResult {
        self.device
            .lock()
            .write_blocks(sector as usize, in_buf)
            .map_err(as_driver_error)
    }
}

impl<H: Hal, T: Transport> Device for VirtIoBlkDev<H, T> {
    fn name(&self) -> &str {
        "virtio-blk"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Block
    }
}

impl<H: Hal, T: Transport> BlockDevice for VirtIoBlkDev<H, T> {
    #[inline]
    fn num_blocks(&self) -> u64 {
        self.num_blocks
    }

    #[inline]
    fn block_size(&self) -> usize {
        self.sector_size
    }

    fn read_block(&self, block_id: u64, buf: &mut [u8]) -> DriverResult {
        self.read_sector(block_id, buf)
    }

    fn write_block(&self, block_id: u64, buf: &[u8]) -> DriverResult {
        self.write_sector(block_id, buf)
    }

    fn flush(&self) -> DriverResult {
        self.device.lock().flush().map_err(as_driver_error)
    }
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert, assert_eq, def_test};

    use super::*;
    use crate::mock_virtio::{MockHal, MockTransport};

    #[def_test]
    fn test_virtio_blk_init_failure_handling() {
        let transport = MockTransport::new();
        let dev = VirtIoBlkDev::<MockHal, MockTransport>::try_new(transport);

        if let Ok(d) = dev {
            assert_eq!(d.name(), "virtio-blk");
            assert_eq!(d.device_kind(), DeviceKind::Block);
            assert_eq!(d.block_size(), 512);
        } else {
            assert!(dev.is_err());
        }
    }

    #[def_test]
    fn test_virtio_blk_concurrency_traits() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<VirtIoBlkDev<MockHal, MockTransport>>();
    }
}

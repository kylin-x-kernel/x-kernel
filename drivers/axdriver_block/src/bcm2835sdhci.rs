//! SD card driver for raspi4

use bcm2835_sdhci::{
    Bcm2835SDhci::{BLOCK_SIZE, EmmcCtl},
    SDHCIError,
};
use driver_base::{DeviceKind, DriverError, DriverOps, DriverResult};

use crate::BlockDriverOps;

/// BCM2835 SDHCI driver (Raspberry Pi SD card).
pub struct SDHCIDriver(EmmcCtl);

impl SDHCIDriver {
    /// Initialize the SDHCI driver, returns `Ok` if successful.
    pub fn try_new() -> DriverResult<SDHCIDriver> {
        let mut ctrl = EmmcCtl::new();
        if ctrl.init() == 0 {
            log::info!("BCM2835 sdhci: successfully initialized");
            Ok(SDHCIDriver(ctrl))
        } else {
            log::warn!("BCM2835 sdhci: init failed");
            Err(DriverError::Io)
        }
    }
}

fn deal_sdhci_err(err: SDHCIError) -> DriverError {
    match err {
        SDHCIError::Io => DriverError::Io,
        SDHCIError::AlreadyExists => DriverError::AlreadyExists,
        SDHCIError::Again => DriverError::WouldBlock,
        SDHCIError::BadState => DriverError::BadState,
        SDHCIError::InvalidParam => DriverError::InvalidInput,
        SDHCIError::NoMemory => DriverError::NoMemory,
        SDHCIError::ResourceBusy => DriverError::ResourceBusy,
        SDHCIError::Unsupported => DriverError::Unsupported,
    }
}

impl DriverOps for SDHCIDriver {
    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Block
    }

    fn name(&self) -> &str {
        "bcm2835_sdhci"
    }
}

impl BlockDriverOps for SDHCIDriver {
    fn read_block(&mut self, block_id: u64, buf: &mut [u8]) -> DriverResult {
        if buf.len() < BLOCK_SIZE {
            return Err(DriverError::InvalidInput);
        }
        let (prefix, aligned_buf, suffix) = unsafe { buf.align_to_mut::<u32>() };
        if !prefix.is_empty() || !suffix.is_empty() {
            return Err(DriverError::InvalidInput);
        }
        self.0
            .read_block(block_id as u32, 1, aligned_buf)
            .map_err(deal_sdhci_err)
    }

    fn write_block(&mut self, block_id: u64, buf: &[u8]) -> DriverResult {
        if buf.len() < BLOCK_SIZE {
            return Err(DriverError::Io);
        }
        let (prefix, aligned_buf, suffix) = unsafe { buf.align_to::<u32>() };
        if !prefix.is_empty() || !suffix.is_empty() {
            return Err(DriverError::InvalidInput);
        }
        self.0
            .write_block(block_id as u32, 1, aligned_buf)
            .map_err(deal_sdhci_err)
    }

    fn flush(&mut self) -> DriverResult {
        Ok(())
    }

    #[inline]
    fn num_blocks(&self) -> u64 {
        self.0.get_block_num()
    }

    #[inline]
    fn block_size(&self) -> usize {
        self.0.get_block_size()
    }
}

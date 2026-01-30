// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! SD card driver for Raspberry Pi 4 (BCM2835 SDHCI)

use bcm2835_sdhci::{
    Bcm2835SDhci::{BLOCK_SIZE, EmmcCtl},
    SDHCIError,
};
use driver_base::{DeviceKind, DriverError, DriverOps, DriverResult};

use crate::BlockDriverOps;

/// Raspberry Pi 4 SD card driver based on BCM2835 SDHCI controller.
pub struct SDHCIDriver(EmmcCtl);

impl SDHCIDriver {
    /// Creates and initializes the SDHCI driver instance.
    ///
    /// Returns `Ok` if initialization succeeds, or an error if it fails.
    pub fn new() -> DriverResult<SDHCIDriver> {
        let mut controller = EmmcCtl::new();
        if controller.init() == 0 {
            log::info!("SDHCI driver: initialization successful.");
            Ok(SDHCIDriver(controller))
        } else {
            log::warn!("SDHCI driver: initialization failed.");
            Err(DriverError::Io)
        }
    }
}

/// Converts SDHCI specific errors to generalized driver errors.
fn convert_sdhci_error(err: SDHCIError) -> DriverError {
    use SDHCIError::*;
    match err {
        Io => DriverError::Io,
        AlreadyExists => DriverError::AlreadyExists,
        Again => DriverError::WouldBlock,
        BadState => DriverError::BadState,
        InvalidParam => DriverError::InvalidInput,
        NoMemory => DriverError::NoMemory,
        ResourceBusy => DriverError::ResourceBusy,
        Unsupported => DriverError::Unsupported,
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
    fn read_block(&mut self, block_id: u64, buffer: &mut [u8]) -> DriverResult {
        if buffer.len() < BLOCK_SIZE {
            return Err(DriverError::InvalidInput);
        }

        // Ensure buffer alignment to 32-bit boundaries
        let (prefix, aligned_buffer, suffix) = unsafe { buffer.align_to_mut::<u32>() };
        if !prefix.is_empty() || !suffix.is_empty() {
            return Err(DriverError::InvalidInput);
        }

        self.0
            .read_block(block_id as u32, 1, aligned_buffer)
            .map_err(convert_sdhci_error)
    }

    fn write_block(&mut self, block_id: u64, buffer: &[u8]) -> DriverResult {
        if buffer.len() < BLOCK_SIZE {
            return Err(DriverError::Io);
        }

        // Ensure buffer alignment to 32-bit boundaries
        let (prefix, aligned_buffer, suffix) = unsafe { buffer.align_to::<u32>() };
        if !prefix.is_empty() || !suffix.is_empty() {
            return Err(DriverError::InvalidInput);
        }

        self.0
            .write_block(block_id as u32, 1, aligned_buffer)
            .map_err(convert_sdhci_error)
    }

    fn flush(&mut self) -> DriverResult {
        Ok(())
    }

    fn num_blocks(&self) -> u64 {
        self.0.get_block_num()
    }

    fn block_size(&self) -> usize {
        self.0.get_block_size()
    }
}

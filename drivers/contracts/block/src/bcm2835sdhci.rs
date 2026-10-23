// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! SD card driver for Raspberry Pi 4 (BCM2835 SDHCI)
//!
//! The pinned SDK copies CPU words to/from its DATA FIFO (PIO). The caller's
//! CPU buffer is not a DMA target; ordinary cached word staging is sufficient
//! for this path and supplies no device cache-maintenance guarantee.

#[cfg(not(unittest))]
use bcm2835_sdhci::{
    Bcm2835SDhci::{BLOCK_SIZE, EmmcCtl},
    SDHCIError,
};
use device_res::CpuMemoryOp;
use driver_base::{Device, DeviceKind, DriverError, DriverResult};
use kspin::SpinNoIrq;

use crate::BlockDeviceOperations;
#[cfg(unittest)]
use crate::bcm_controller_fixture::{BLOCK_SIZE, EmmcCtl, SdhciError as SDHCIError};

/// Raspberry Pi 4 SD card driver based on BCM2835 SDHCI controller.
///
/// The `SpinNoIrq` lock serializes hardware access so the device can be
/// shared behind a `&self` interface. `num_blocks` and `block_size` cache
/// the device geometry so queries do not need to take the lock.
/// Word-aligned CPU buffers are passed directly; other addresses use owned
/// block-layer staging before acquiring the controller lock. Controller calls
/// transfer one complete sector, including for a multi-sector request.
#[cfg_attr(
    unittest,
    expect(
        clippy::upper_case_acronyms,
        reason = "CG-024-lint-exceptions: keep the existing legacy driver type name and probe API \
                  while compiling its adapter in kernel UT"
    )
)]
pub struct SDHCIDriver {
    inner: SpinNoIrq<EmmcCtl>,
    num_blocks: u64,
    block_size: usize,
    buffer_provider: &'static dyn CpuMemoryOp,
}

impl SDHCIDriver {
    /// Creates and initializes the SDHCI driver instance.
    ///
    /// Returns `Ok` if initialization succeeds, or an error if it fails.
    pub fn new(buffer_provider: &'static dyn CpuMemoryOp) -> DriverResult<SDHCIDriver> {
        let mut controller = EmmcCtl::new();
        if controller.init() == 0 {
            log::info!("SDHCI driver: initialization successful.");
            let num_blocks = controller.get_block_num();
            let block_size = controller.get_block_size();
            Ok(SDHCIDriver {
                inner: SpinNoIrq::new(controller),
                num_blocks,
                block_size,
                buffer_provider,
            })
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

impl Device for SDHCIDriver {
    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Block
    }

    fn name(&self) -> &str {
        "bcm2835_sdhci"
    }
}

impl BlockDeviceOperations for SDHCIDriver {
    fn read_block(&self, block_id: u64, buffer: &mut [u8]) -> DriverResult {
        if self.block_size != BLOCK_SIZE {
            return Err(DriverError::Unsupported);
        }
        let capacity = self.num_blocks.min(u64::from(u32::MAX) + 1);
        crate::word_io::read_blocks_with_provider(
            self.buffer_provider,
            block_id,
            BLOCK_SIZE,
            capacity,
            buffer,
            |block, bytes| {
                // SAFETY: word_io supplies a live, exclusive byte slice at a u32
                // aligned address with whole-sector length. All bytes are already
                // initialized, all bit patterns are valid u32 values, and the word
                // view remains inside this borrow until the synchronous I/O ends.
                let (prefix, words, suffix) = unsafe { bytes.align_to_mut::<u32>() };
                if !prefix.is_empty() || !suffix.is_empty() {
                    return Err(DriverError::InvalidInput);
                }
                let mut controller = self.inner.lock();
                for (index, sector) in words.chunks_exact_mut(BLOCK_SIZE / 4).enumerate() {
                    let sector_id = u32::try_from(block + index as u64)
                        .map_err(|_| DriverError::InvalidInput)?;
                    controller
                        .read_block(sector_id, 1, sector)
                        .map_err(convert_sdhci_error)?;
                }
                Ok(())
            },
        )
    }

    fn write_block(&self, block_id: u64, buffer: &[u8]) -> DriverResult {
        if self.block_size != BLOCK_SIZE {
            return Err(DriverError::Unsupported);
        }
        let capacity = self.num_blocks.min(u64::from(u32::MAX) + 1);
        crate::word_io::write_blocks_with_provider(
            self.buffer_provider,
            block_id,
            BLOCK_SIZE,
            capacity,
            buffer,
            |block, bytes| {
                // SAFETY: word_io guarantees u32 address alignment and an initialized
                // whole-sector slice. The shared word view covers that same live
                // allocation, cannot outlive it and is not mutated by this path.
                let (prefix, words, suffix) = unsafe { bytes.align_to::<u32>() };
                if !prefix.is_empty() || !suffix.is_empty() {
                    return Err(DriverError::InvalidInput);
                }
                let mut controller = self.inner.lock();
                for (index, sector) in words.chunks_exact(BLOCK_SIZE / 4).enumerate() {
                    let sector_id = u32::try_from(block + index as u64)
                        .map_err(|_| DriverError::InvalidInput)?;
                    controller
                        .write_block(sector_id, 1, sector)
                        .map_err(convert_sdhci_error)?;
                }
                Ok(())
            },
        )
    }

    fn flush(&self) -> DriverResult {
        Ok(())
    }

    fn num_blocks(&self) -> u64 {
        self.num_blocks
    }

    fn block_size(&self) -> usize {
        self.block_size
    }
}

unittest::path_mod_test!(pub mod tests_bcm2835sdhci, "../ut-tests/bcm2835sdhci.rs");
unittest::path_mod_test!(mod word_io_tests, "../ut-tests/bcm_word_io.rs");

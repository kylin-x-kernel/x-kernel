// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! SD/MMC driver based on SDIO.

use driver_base::{Device, DeviceKind, DriverError, DriverResult};
use kspin::SpinNoIrq;
use simple_sdmmc::SdMmc;

use crate::BlockDeviceOperations;

/// A SD/MMC driver.
///
/// The `SpinNoIrq` lock serializes hardware access so the device can be
/// shared behind a `&self` interface. `num_blocks` caches the disk capacity
/// so size queries do not need to take the lock.
pub struct SdMmcDriver {
    inner: SpinNoIrq<SdMmc>,
    num_blocks: u64,
}

impl SdMmcDriver {
    /// Creates a new [`SdMmcDriver`] from the given base address.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `base` is a valid pointer to the SD/MMC controller's
    /// register block and that no other code is concurrently accessing the same hardware.
    pub unsafe fn new(base: usize) -> Self {
        // SAFETY: the caller upholds the `# Safety` contract of this function:
        // `base` points to the SD/MMC controller's register block and no other
        // code accesses that hardware concurrently, which is exactly the
        // contract `simple_sdmmc::SdMmc::new` requires.
        let sdmmc = unsafe { SdMmc::new(base) };
        let num_blocks = sdmmc.num_blocks();
        Self {
            inner: SpinNoIrq::new(sdmmc),
            num_blocks,
        }
    }
}

impl Device for SdMmcDriver {
    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Block
    }

    fn name(&self) -> &str {
        "sdmmc"
    }
}

impl BlockDeviceOperations for SdMmcDriver {
    fn num_blocks(&self) -> u64 {
        self.num_blocks
    }

    fn block_size(&self) -> usize {
        SdMmc::BLOCK_SIZE
    }
}

impl SdMmcDriver {
    fn read_block(&self, block_id: u64, buf: &mut [u8]) -> DriverResult {
        let (blocks, remainder) = buf.as_chunks_mut::<{ SdMmc::BLOCK_SIZE }>();

        if !remainder.is_empty() {
            return Err(DriverError::InvalidInput);
        }

        let mut dev = self.inner.lock();
        for (i, block) in blocks.iter_mut().enumerate() {
            dev.read_block(block_id as u32 + i as u32, block);
        }

        Ok(())
    }

    fn write_block(&self, block_id: u64, buf: &[u8]) -> DriverResult {
        let (blocks, remainder) = buf.as_chunks::<{ SdMmc::BLOCK_SIZE }>();

        if !remainder.is_empty() {
            return Err(DriverError::InvalidInput);
        }

        let mut dev = self.inner.lock();
        for (i, block) in blocks.iter().enumerate() {
            dev.write_block(block_id as u32 + i as u32, block);
        }

        Ok(())
    }

    fn flush(&self) -> DriverResult {
        Ok(())
    }
}

impl crate::BlkMqOperations for SdMmcDriver {
    fn queue_rq(&self, mut request: crate::Request) {
        let block_id = request.sector() / (self.block_size() as u64 / 512);
        let result = match request.operation() {
            crate::BioOperation::Read => {
                request.read_contiguous(|bytes| self.read_block(block_id, bytes))
            }
            crate::BioOperation::Write => {
                request.write_contiguous(|bytes| self.write_block(block_id, bytes))
            }
            crate::BioOperation::Flush => self.flush(),
        };
        request.complete(result);
    }
}

unittest::path_mod_test!(tests_sdmmc, "../ut-tests/sdmmc.rs");

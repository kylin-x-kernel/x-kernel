// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! A RAM disk driver implemented with a static slice as storage.

use core::ops::{Deref, DerefMut};

use driver_base::{DeviceKind, DriverError, DriverOps, DriverResult};

use crate::BlockDriverOps;

const BLOCK_SIZE: usize = 512;

/// RAM disk structure backed by a static mutable slice.
#[derive(Default)]
pub struct RamDisk(&'static mut [u8]);

impl RamDisk {
    /// Constructs a new RAM disk from the provided static buffer.
    ///
    /// # Panics
    /// Panics if the buffer is not aligned to the block size or its size is
    /// not a multiple of the block size.
    pub fn new(buffer: &'static mut [u8]) -> Self {
        assert_eq!(
            buffer.as_ptr().addr() & (BLOCK_SIZE - 1),
            0,
            "Buffer not aligned to block size."
        );
        assert_eq!(
            buffer.len() % BLOCK_SIZE,
            0,
            "Buffer size is not a multiple of block size."
        );
        RamDisk(buffer)
    }
}

impl Deref for RamDisk {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.0
    }
}

impl DerefMut for RamDisk {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0
    }
}

impl DriverOps for RamDisk {
    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Block
    }

    fn name(&self) -> &str {
        "ramdisk"
    }
}

impl BlockDriverOps for RamDisk {
    /// Returns the number of blocks the RAM disk can hold.
    #[inline]
    fn num_blocks(&self) -> u64 {
        (self.len() / BLOCK_SIZE) as u64
    }

    /// Returns the block size of the RAM disk.
    #[inline]
    fn block_size(&self) -> usize {
        BLOCK_SIZE
    }

    /// Reads a single block from the RAM disk into the provided buffer.
    fn read_block(&mut self, block_id: u64, buffer: &mut [u8]) -> DriverResult {
        if buffer.len() % BLOCK_SIZE != 0 {
            return Err(DriverError::InvalidInput);
        }

        let offset = block_id as usize * BLOCK_SIZE;
        if offset + buffer.len() > self.len() {
            return Err(DriverError::Io);
        }

        buffer.copy_from_slice(&self[offset..offset + buffer.len()]);
        Ok(())
    }

    /// Writes a single block to the RAM disk from the provided buffer.
    fn write_block(&mut self, block_id: u64, buffer: &[u8]) -> DriverResult {
        if buffer.len() % BLOCK_SIZE != 0 {
            return Err(DriverError::InvalidInput);
        }

        let offset = block_id as usize * BLOCK_SIZE;
        if offset + buffer.len() > self.len() {
            return Err(DriverError::Io);
        }

        self[offset..offset + buffer.len()].copy_from_slice(buffer);
        Ok(())
    }

    /// No operation needed for flushing RAM disk.
    fn flush(&mut self) -> DriverResult {
        Ok(())
    }
}

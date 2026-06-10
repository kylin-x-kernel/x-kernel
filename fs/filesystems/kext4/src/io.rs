// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{sync::Arc, vec};

use block::BlockDevice;

use crate::{Ext4Error, Ext4Result, FilesystemBlock};

/// Adapts device blocks to the ext4 filesystem block address space.
pub struct FilesystemDevice {
    device: Arc<dyn BlockDevice>,
    device_block_size: usize,
    filesystem_block_size: usize,
    blocks_per_filesystem_block: u64,
    filesystem_blocks: u64,
}

impl FilesystemDevice {
    pub(crate) fn open(
        device: Arc<dyn BlockDevice>,
        filesystem_block_size: usize,
        filesystem_blocks: u64,
    ) -> Ext4Result<Self> {
        let device_block_size = device.block_size();
        if device_block_size == 0
            || !device_block_size.is_power_of_two()
            || !filesystem_block_size.is_multiple_of(device_block_size)
        {
            return Err(Ext4Error::InvalidDeviceBlockSize(device_block_size));
        }

        let blocks_per_filesystem_block = u64::try_from(filesystem_block_size / device_block_size)
            .map_err(|_| Ext4Error::Overflow)?;
        let required_device_blocks = filesystem_blocks
            .checked_mul(blocks_per_filesystem_block)
            .ok_or(Ext4Error::Overflow)?;
        if required_device_blocks > device.num_blocks() {
            return Err(Ext4Error::OutOfBounds);
        }

        Ok(Self {
            device,
            device_block_size,
            filesystem_block_size,
            blocks_per_filesystem_block,
            filesystem_blocks,
        })
    }

    pub(crate) fn read_bytes(
        device: &dyn BlockDevice,
        byte_offset: u64,
        output: &mut [u8],
    ) -> Ext4Result<()> {
        if output.is_empty() {
            return Ok(());
        }

        let device_block_size = device.block_size();
        if device_block_size == 0 || !device_block_size.is_power_of_two() {
            return Err(Ext4Error::InvalidDeviceBlockSize(device_block_size));
        }
        let block_size = u64::try_from(device_block_size).map_err(|_| Ext4Error::Overflow)?;
        let output_len = u64::try_from(output.len()).map_err(|_| Ext4Error::Overflow)?;
        let end = byte_offset
            .checked_add(output_len)
            .ok_or(Ext4Error::Overflow)?;
        let capacity = device
            .num_blocks()
            .checked_mul(block_size)
            .ok_or(Ext4Error::Overflow)?;
        if end > capacity {
            return Err(Ext4Error::OutOfBounds);
        }

        let first_block = byte_offset / block_size;
        let last_block = end.checked_add(block_size - 1).ok_or(Ext4Error::Overflow)? / block_size;
        let block_count = last_block
            .checked_sub(first_block)
            .ok_or(Ext4Error::Overflow)?;
        let read_len = usize::try_from(
            block_count
                .checked_mul(block_size)
                .ok_or(Ext4Error::Overflow)?,
        )
        .map_err(|_| Ext4Error::Overflow)?;
        let mut bounce = vec![0; read_len];
        device.read_block(first_block, &mut bounce)?;

        let start = usize::try_from(byte_offset % block_size).map_err(|_| Ext4Error::Overflow)?;
        let source_end = start.checked_add(output.len()).ok_or(Ext4Error::Overflow)?;
        output.copy_from_slice(
            bounce
                .get(start..source_end)
                .ok_or(Ext4Error::OutOfBounds)?,
        );
        Ok(())
    }

    /// Returns the ext4 filesystem block size.
    pub const fn block_size(&self) -> usize {
        self.filesystem_block_size
    }

    /// Returns the number of addressable ext4 filesystem blocks.
    pub const fn block_count(&self) -> u64 {
        self.filesystem_blocks
    }

    /// Reads one or more complete contiguous filesystem blocks.
    pub fn read_blocks(
        &self,
        start: FilesystemBlock,
        block_count: u32,
        output: &mut [u8],
    ) -> Ext4Result<()> {
        let expected = usize::try_from(block_count)
            .map_err(|_| Ext4Error::Overflow)?
            .checked_mul(self.filesystem_block_size)
            .ok_or(Ext4Error::Overflow)?;
        if output.len() != expected {
            return Err(Ext4Error::InvalidBufferLength {
                expected,
                actual: output.len(),
            });
        }

        let end = start
            .get()
            .checked_add(u64::from(block_count))
            .ok_or(Ext4Error::Overflow)?;
        if end > self.filesystem_blocks {
            return Err(Ext4Error::OutOfBounds);
        }

        let device_block = start
            .get()
            .checked_mul(self.blocks_per_filesystem_block)
            .ok_or(Ext4Error::Overflow)?;
        self.device.read_block(device_block, output)?;
        Ok(())
    }

    /// Returns the underlying hardware block size.
    pub const fn device_block_size(&self) -> usize {
        self.device_block_size
    }
}

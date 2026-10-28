// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{sync::Arc, vec};

#[cfg(unittest)]
use block::BlockDeviceOperations;

use crate::{
    Ext4Error, Ext4Result, FilesystemBlock,
    jbd2::{JournalReplayBlockWriter, JournalTargetBlock},
};

/// Adapts device blocks to the ext4 filesystem block address space.
pub(crate) struct FilesystemDevice {
    device: Arc<block::BlockDevice>,
    filesystem_block_size: usize,
    filesystem_blocks: u64,
}

impl FilesystemDevice {
    pub(crate) fn open(
        device: Arc<block::BlockDevice>,
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
        if required_device_blocks > (device.size() / device.block_size() as u64) {
            return Err(Ext4Error::OutOfBounds);
        }

        Ok(Self {
            device,
            filesystem_block_size,
            filesystem_blocks,
        })
    }

    pub(crate) fn read_bytes(
        device: &Arc<block::BlockDevice>,
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
        let capacity = device.size();
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

    pub(crate) const fn block_size(&self) -> usize {
        self.filesystem_block_size
    }

    pub(crate) fn read_blocks(
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
            .checked_mul((self.filesystem_block_size / self.device.block_size()) as u64)
            .ok_or(Ext4Error::Overflow)?;
        self.device.read_block(device_block, output)?;
        Ok(())
    }

    /// Writes one or more complete contiguous filesystem blocks.
    pub(crate) fn write_contiguous_blocks(
        &self,
        start: FilesystemBlock,
        block_count: u32,
        input: &[u8],
    ) -> Ext4Result<()> {
        let expected = usize::try_from(block_count)
            .map_err(|_| Ext4Error::Overflow)?
            .checked_mul(self.filesystem_block_size)
            .ok_or(Ext4Error::Overflow)?;
        if input.len() != expected {
            return Err(Ext4Error::InvalidBufferLength {
                expected,
                actual: input.len(),
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
            .checked_mul((self.filesystem_block_size / self.device.block_size()) as u64)
            .ok_or(Ext4Error::Overflow)?;
        let device_block_count = u64::from(block_count)
            .checked_mul((self.filesystem_block_size / self.device.block_size()) as u64)
            .ok_or(Ext4Error::Overflow)?;
        self.write_contiguous_device_blocks(device_block, device_block_count, input)?;
        Ok(())
    }

    /// Prepares an owned, block-aligned write without submitting it.
    /// BIO addresses are always 512-byte sectors, independent of device blocks.
    pub(crate) fn prepare_owned_write(
        &self,
        start: FilesystemBlock,
        input: alloc::boxed::Box<[u8]>,
    ) -> Ext4Result<(block::Bio, block::completion::BioWait)> {
        if input.is_empty() || !input.len().is_multiple_of(self.filesystem_block_size) {
            return Err(Ext4Error::OutOfBounds);
        }
        let blocks = (input.len() / self.filesystem_block_size) as u64;
        let end = start.get().checked_add(blocks).ok_or(Ext4Error::Overflow)?;
        if end > self.filesystem_blocks {
            return Err(Ext4Error::OutOfBounds);
        }
        let byte_offset = start
            .get()
            .checked_mul(self.filesystem_block_size as u64)
            .ok_or(Ext4Error::Overflow)?;
        if !byte_offset.is_multiple_of(512) {
            return Err(Ext4Error::OutOfBounds);
        }
        block::completion::BioWait::prepare(
            self.device.clone(),
            byte_offset / 512,
            block::BioOperation::Write,
            vec![block::BioVec::new(input)],
        )
        .map_err(|(error, _)| Ext4Error::Device(error))
    }

    pub(crate) fn submit_owned_write(&self, bio: block::Bio) {
        self.device.submit_bio(bio);
    }

    /// Flushes pending device writes.
    pub(crate) fn flush(&self) -> Ext4Result<()> {
        self.device.flush().map_err(Ext4Error::Device)
    }

    fn write_contiguous_device_blocks(
        &self,
        start: u64,
        block_count: u64,
        input: &[u8],
    ) -> Ext4Result<()> {
        let expected = usize::try_from(block_count)
            .map_err(|_| Ext4Error::Overflow)?
            .checked_mul(self.device.block_size())
            .ok_or(Ext4Error::Overflow)?;
        if input.len() != expected {
            return Err(Ext4Error::InvalidBufferLength {
                expected,
                actual: input.len(),
            });
        }
        let end = start.checked_add(block_count).ok_or(Ext4Error::Overflow)?;
        if end > (self.device.size() / self.device.block_size() as u64) {
            return Err(Ext4Error::OutOfBounds);
        }

        self.device
            .write_block(start, input)
            .map_err(Ext4Error::Device)
    }
}

impl JournalReplayBlockWriter for FilesystemDevice {
    fn write_replay_block(&self, block: JournalTargetBlock, input: &[u8]) -> Ext4Result<()> {
        self.write_contiguous_blocks(FilesystemBlock::new(block.get()), 1, input)
    }

    fn flush_replay(&self) -> Ext4Result<()> {
        self.flush()
    }
}

#[cfg(unittest)]
pub(crate) fn test_block_device<T: block::BioOperations + 'static>(
    device: Arc<T>,
) -> Arc<block::BlockDevice> {
    block::Gendisk::alloc(
        alloc::string::String::from("ext4-test"),
        0,
        0,
        1,
        alloc::boxed::Box::new(device),
    )
    .unwrap()
    .part0()
}

unittest::path_mod_test!(tests, "../ut-tests/io/tests.rs");

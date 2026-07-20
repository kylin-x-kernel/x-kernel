// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{sync::Arc, vec};

use block::BlockDevice;

use crate::{
    Ext4Error, Ext4Result, FilesystemBlock,
    jbd2::{JournalReplayBlockWriter, JournalTargetBlock},
};

/// Adapts device blocks to the ext4 filesystem block address space.
pub(crate) struct FilesystemDevice {
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
            .checked_mul(self.blocks_per_filesystem_block)
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
            .checked_mul(self.blocks_per_filesystem_block)
            .ok_or(Ext4Error::Overflow)?;
        let device_block_count = u64::from(block_count)
            .checked_mul(self.blocks_per_filesystem_block)
            .ok_or(Ext4Error::Overflow)?;
        self.write_contiguous_device_blocks(device_block, device_block_count, input)?;
        Ok(())
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
            .checked_mul(self.device_block_size)
            .ok_or(Ext4Error::Overflow)?;
        if input.len() != expected {
            return Err(Ext4Error::InvalidBufferLength {
                expected,
                actual: input.len(),
            });
        }
        let end = start.checked_add(block_count).ok_or(Ext4Error::Overflow)?;
        if end > self.device.num_blocks() {
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

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;
    use std::sync::Mutex;

    use block::{Device, DeviceKind, DriverError, DriverResult};

    use super::*;

    const DEVICE_BLOCK_SIZE: usize = 512;

    struct MemoryBlockDevice {
        bytes: Mutex<Vec<u8>>,
        write_lengths: Mutex<Vec<usize>>,
        is_flushed: Mutex<bool>,
    }

    impl MemoryBlockDevice {
        fn new(device_blocks: usize) -> Self {
            Self {
                bytes: Mutex::new(vec![0; device_blocks * DEVICE_BLOCK_SIZE]),
                write_lengths: Mutex::new(Vec::new()),
                is_flushed: Mutex::new(false),
            }
        }
    }

    impl Device for MemoryBlockDevice {
        fn name(&self) -> &str {
            "kext4-memory-block-device"
        }

        fn device_kind(&self) -> DeviceKind {
            DeviceKind::Block
        }
    }

    impl BlockDevice for MemoryBlockDevice {
        fn num_blocks(&self) -> u64 {
            (self.bytes.lock().unwrap().len() / DEVICE_BLOCK_SIZE) as u64
        }

        fn block_size(&self) -> usize {
            DEVICE_BLOCK_SIZE
        }

        fn read_block(&self, block_id: u64, output: &mut [u8]) -> DriverResult {
            let start = usize::try_from(block_id)
                .map_err(|_| DriverError::InvalidInput)?
                .checked_mul(DEVICE_BLOCK_SIZE)
                .ok_or(DriverError::InvalidInput)?;
            let end = start
                .checked_add(output.len())
                .ok_or(DriverError::InvalidInput)?;
            output.copy_from_slice(
                self.bytes
                    .lock()
                    .unwrap()
                    .get(start..end)
                    .ok_or(DriverError::InvalidInput)?,
            );
            Ok(())
        }

        fn write_block(&self, block_id: u64, input: &[u8]) -> DriverResult {
            self.write_lengths.lock().unwrap().push(input.len());
            let start = usize::try_from(block_id)
                .map_err(|_| DriverError::InvalidInput)?
                .checked_mul(DEVICE_BLOCK_SIZE)
                .ok_or(DriverError::InvalidInput)?;
            let end = start
                .checked_add(input.len())
                .ok_or(DriverError::InvalidInput)?;
            self.bytes
                .lock()
                .unwrap()
                .get_mut(start..end)
                .ok_or(DriverError::InvalidInput)?
                .copy_from_slice(input);
            Ok(())
        }

        fn flush(&self) -> DriverResult {
            *self.is_flushed.lock().unwrap() = true;
            Ok(())
        }
    }

    #[test]
    fn writes_filesystem_blocks_through_device_block_mapping() {
        let device = Arc::new(MemoryBlockDevice::new(8));
        let filesystem = FilesystemDevice::open(device.clone(), 1024, 4).unwrap();
        let input = vec![0x5a; 1024];

        filesystem
            .write_contiguous_blocks(FilesystemBlock::new(2), 1, &input)
            .unwrap();

        let bytes = device.bytes.lock().unwrap();
        assert_eq!(&bytes[2048..3072], input.as_slice());
        assert_eq!(device.write_lengths.lock().unwrap().as_slice(), &[1024]);
    }

    #[test]
    fn rejects_partial_contiguous_filesystem_block_write() {
        let device = Arc::new(MemoryBlockDevice::new(8));
        let filesystem = FilesystemDevice::open(device, 1024, 4).unwrap();

        assert_eq!(
            filesystem.write_contiguous_blocks(FilesystemBlock::new(1), 1, &[0; 512]),
            Err(Ext4Error::InvalidBufferLength {
                expected: 1024,
                actual: 512,
            })
        );
    }

    #[test]
    fn replay_writer_writes_one_block_and_flushes() {
        let device = Arc::new(MemoryBlockDevice::new(8));
        let filesystem = FilesystemDevice::open(device.clone(), 1024, 4).unwrap();
        let input = vec![0xa5; 1024];

        filesystem
            .write_replay_block(JournalTargetBlock::new(1), &input)
            .unwrap();
        filesystem.flush_replay().unwrap();

        let bytes = device.bytes.lock().unwrap();
        assert_eq!(&bytes[1024..2048], input.as_slice());
        assert!(*device.is_flushed.lock().unwrap());
    }
}

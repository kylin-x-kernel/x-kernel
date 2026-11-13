// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{sync::Arc, vec, vec::Vec};

#[cfg(unittest)]
use block::BlockDeviceOperations;

use crate::{
    Ext4Error, Ext4Result, FilesystemBlock,
    jbd2::{JournalReplayBlockWriter, JournalTargetBlock},
};

/// Validates ext4 filesystem ranges and submits them as sector-addressed bios.
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
        if filesystem_block_size == 0 || !filesystem_block_size.is_multiple_of(device_block_size) {
            return Err(Ext4Error::InvalidDeviceBlockSize(device_block_size));
        }

        let filesystem_bytes = filesystem_blocks
            .checked_mul(filesystem_block_size as u64)
            .ok_or(Ext4Error::Overflow)?;
        if filesystem_bytes > device.size() {
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
        let block_size = u64::try_from(device_block_size).map_err(|_| Ext4Error::Overflow)?;
        let output_len = u64::try_from(output.len()).map_err(|_| Ext4Error::Overflow)?;
        let end = byte_offset
            .checked_add(output_len)
            .ok_or(Ext4Error::Overflow)?;
        let capacity = device.size();
        if end > capacity {
            return Err(Ext4Error::OutOfBounds);
        }

        if byte_offset.is_multiple_of(block_size) && output.len().is_multiple_of(device_block_size)
        {
            device.read_sectors(byte_offset / u64::from(block::SECTOR_SIZE), output)?;
            return Ok(());
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

    /// Submits complete filesystem blocks in their original memory owners.
    /// The callback runs after device access ends and must not sleep. Before
    /// submission, a geometry error drops the owners and returns the error.
    pub(crate) fn submit_read(
        &self,
        start: FilesystemBlock,
        vectors: Vec<block::BioVec>,
        end_io: impl FnOnce(block::DriverResult, Vec<block::BioVec>) + Send + 'static,
    ) -> Ext4Result<()> {
        let len = vectors.iter().try_fold(0usize, |len, vector| {
            len.checked_add(vector.bytes().len())
                .ok_or(Ext4Error::Overflow)
        })?;
        if len == 0 || !len.is_multiple_of(self.filesystem_block_size) {
            return Err(Ext4Error::InvalidBufferLength {
                expected: self.filesystem_block_size,
                actual: len,
            });
        }
        let blocks = (len / self.filesystem_block_size) as u64;
        if start.get().checked_add(blocks).ok_or(Ext4Error::Overflow)? > self.filesystem_blocks {
            return Err(Ext4Error::OutOfBounds);
        }
        // Bio sectors are device-view relative 512-byte units. The block core
        // applies a partition offset once, independently of filesystem geometry.
        let sector = start
            .get()
            .checked_mul((self.filesystem_block_size / block::SECTOR_SIZE as usize) as u64)
            .ok_or(Ext4Error::Overflow)?;
        self.device.submit_bio(block::Bio::new(
            self.device.clone(),
            sector,
            block::BioOperation::Read,
            vectors,
            end_io,
        ));
        Ok(())
    }

    pub(crate) fn read_blocks(
        &self,
        start: FilesystemBlock,
        block_count: u32,
        output: &mut [u8],
    ) -> Ext4Result<()> {
        let sector = self.validate_blocks(start, u64::from(block_count), output.len())?;
        self.device.read_sectors(sector, output)?;
        Ok(())
    }

    /// Writes one or more complete contiguous filesystem blocks.
    pub(crate) fn write_contiguous_blocks(
        &self,
        start: FilesystemBlock,
        block_count: u32,
        input: &[u8],
    ) -> Ext4Result<()> {
        let sector = self.validate_blocks(start, u64::from(block_count), input.len())?;
        self.device.write_sectors(sector, input)?;
        Ok(())
    }

    /// Checks filesystem bounds; queue geometry and media bounds belong to block core.
    fn validate_blocks(
        &self,
        start: FilesystemBlock,
        block_count: u64,
        length_bytes: usize,
    ) -> Ext4Result<u64> {
        let expected = usize::try_from(block_count)
            .map_err(|_| Ext4Error::Overflow)?
            .checked_mul(self.filesystem_block_size)
            .ok_or(Ext4Error::Overflow)?;
        if length_bytes != expected {
            return Err(Ext4Error::InvalidBufferLength {
                expected,
                actual: length_bytes,
            });
        }
        let end = start
            .get()
            .checked_add(block_count)
            .ok_or(Ext4Error::Overflow)?;
        if end > self.filesystem_blocks {
            return Err(Ext4Error::OutOfBounds);
        }
        start
            .get()
            .checked_mul((self.filesystem_block_size / block::SECTOR_SIZE as usize) as u64)
            .ok_or(Ext4Error::Overflow)
    }

    /// Prepares an owned, block-aligned write without submitting it.
    /// BIO addresses are always 512-byte sectors, independent of device blocks.
    pub(crate) fn prepare_owned_write(
        &self,
        start: FilesystemBlock,
        input: alloc::boxed::Box<[u8]>,
        complete: impl FnOnce(block::DriverResult, alloc::vec::Vec<block::BioVec>) + Send + 'static,
    ) -> Ext4Result<block::Bio> {
        let sector = self.owned_write_sector(start, input.len())?;
        Ok(block::Bio::new(
            self.device.clone(),
            sector,
            block::BioOperation::Write,
            vec![block::BioVec::new(input)],
            complete,
        ))
    }

    /// Prepares a task-bound result owner for synchronous, pipelined writeback.
    pub(crate) fn prepare_waited_write(
        &self,
        start: FilesystemBlock,
        input: alloc::boxed::Box<[u8]>,
    ) -> Ext4Result<(block::Bio, block::completion::BioWait)> {
        let sector = self.owned_write_sector(start, input.len())?;
        block::completion::BioWait::prepare(
            self.device.clone(),
            sector,
            block::BioOperation::Write,
            vec![block::BioVec::new(input)],
        )
        .map_err(|(error, _)| Ext4Error::Device(error))
    }

    fn owned_write_sector(&self, start: FilesystemBlock, input_len: usize) -> Ext4Result<u64> {
        if input_len == 0 || !input_len.is_multiple_of(self.filesystem_block_size) {
            return Err(Ext4Error::OutOfBounds);
        }
        let blocks = (input_len / self.filesystem_block_size) as u64;
        self.validate_blocks(start, blocks, input_len)
    }

    pub(crate) fn submit_owned_write(&self, bio: block::Bio) {
        self.device.submit_bio(bio);
    }

    /// Flushes pending device writes.
    pub(crate) fn flush(&self) -> Ext4Result<()> {
        self.device.flush().map_err(Ext4Error::Device)
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
#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
)]
#[allow(
    unknown_lints,
    std_box_in_kernel_tests,
    reason = "CG-049-no-std-box-in-kernel-tests: Gendisk::alloc still requires a standard Box \
              driver owner"
)]
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

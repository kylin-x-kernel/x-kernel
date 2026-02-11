// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Ext4 filesystem adapter.
mod fs;
mod inode;
mod util;

pub use fs::*;
pub use inode::*;
#[allow(unused_imports)]
use kdriver::{BlockDevice as KBlockDevice, prelude::BlockDriverOps};
use rsext4::{
    BlockDevice,
    error::{BlockDevError, BlockDevResult},
};

const FS_BLOCK_SIZE: usize = rsext4::BLOCK_SIZE;

/// Block device wrapper implementing the ext4 driver traits.
pub(crate) struct Ext4Disk(KBlockDevice);

impl BlockDevice for Ext4Disk {
    fn write(&mut self, buffer: &[u8], block_id: u32, count: u32) -> BlockDevResult<()> {
        let dev_block = self.0.block_size();
        if !FS_BLOCK_SIZE.is_multiple_of(dev_block) {
            return Err(BlockDevError::InvalidBlockSize {
                size: dev_block,
                expected: FS_BLOCK_SIZE,
            });
        }
        let factor = (FS_BLOCK_SIZE / dev_block) as u64;
        let required_size = FS_BLOCK_SIZE * count as usize;
        if buffer.len() < required_size {
            return Err(BlockDevError::BufferTooSmall {
                provided: buffer.len(),
                required: required_size,
            });
        }
        let start_block = block_id as u64 * factor;
        self.0
            .write_block(start_block, &buffer[..required_size])
            .map_err(|_| BlockDevError::WriteError)
    }

    fn read(&mut self, buffer: &mut [u8], block_id: u32, count: u32) -> BlockDevResult<()> {
        let dev_block = self.0.block_size();
        if !FS_BLOCK_SIZE.is_multiple_of(dev_block) {
            return Err(BlockDevError::InvalidBlockSize {
                size: dev_block,
                expected: FS_BLOCK_SIZE,
            });
        }
        let factor = (FS_BLOCK_SIZE / dev_block) as u64;
        let required_size = FS_BLOCK_SIZE * count as usize;
        if buffer.len() < required_size {
            return Err(BlockDevError::BufferTooSmall {
                provided: buffer.len(),
                required: required_size,
            });
        }
        let start_block = block_id as u64 * factor;
        self.0
            .read_block(start_block, &mut buffer[..required_size])
            .map_err(|_| BlockDevError::ReadError)
    }

    fn open(&mut self) -> BlockDevResult<()> {
        Ok(())
    }

    fn close(&mut self) -> BlockDevResult<()> {
        self.flush()
    }

    fn total_blocks(&self) -> u64 {
        let dev_block = self.0.block_size() as u64;
        let total_bytes = self.0.num_blocks().saturating_mul(dev_block);
        total_bytes / FS_BLOCK_SIZE as u64
    }

    fn block_size(&self) -> u32 {
        FS_BLOCK_SIZE as u32
    }

    fn flush(&mut self) -> BlockDevResult<()> {
        self.0.flush().map_err(|_| BlockDevError::IoError)
    }

    fn is_open(&self) -> bool {
        true
    }

    fn is_readonly(&self) -> bool {
        false
    }
}

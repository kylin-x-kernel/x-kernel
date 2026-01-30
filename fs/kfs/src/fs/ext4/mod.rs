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
use lwext4_rust::{BlockDevice, Ext4Error, Ext4Result, ffi::EIO};

/// Block device wrapper implementing the ext4 driver traits.
pub(crate) struct Ext4Disk(KBlockDevice);

impl BlockDevice for Ext4Disk {
    fn read_blocks(&mut self, block_id: u64, buf: &mut [u8]) -> Ext4Result<usize> {
        self.0
            .read_block(block_id, buf)
            .map_err(|_| Ext4Error::new(EIO as _, None))?;
        Ok(buf.len())
    }

    fn write_blocks(&mut self, block_id: u64, buf: &[u8]) -> Ext4Result<usize> {
        self.0
            .write_block(block_id, buf)
            .map_err(|_| Ext4Error::new(EIO as _, None))?;
        Ok(buf.len())
    }

    fn num_blocks(&self) -> Ext4Result<u64> {
        Ok(self.0.num_blocks())
    }
}

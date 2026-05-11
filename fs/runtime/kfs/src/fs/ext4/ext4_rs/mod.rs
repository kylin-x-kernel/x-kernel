// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Ext4 filesystem adapter (ext4_rs backend).
mod fs;
mod inode;
mod util;

use alloc::{vec, vec::Vec};
use core::cmp::min;

use ext4_rs::{BLOCK_SIZE, BlockDevice};
pub use fs::*;
pub use inode::*;
use kdriver::{BlockDevice as KBlockDevice, prelude::BlockDriverOps};
use kspin::SpinNoPreempt as Mutex;

const FS_BLOCK_SIZE: usize = BLOCK_SIZE;

/// Block device wrapper implementing ext4_rs block device APIs.
pub(crate) struct Ext4Disk {
    inner: Mutex<KBlockDevice>,
    block_size: usize,
}

impl Ext4Disk {
    pub(crate) fn new(dev: KBlockDevice) -> Self {
        let block_size = dev.block_size();
        Self {
            inner: Mutex::new(dev),
            block_size,
        }
    }
}

impl BlockDevice for Ext4Disk {
    fn read_offset(&self, offset: usize) -> Vec<u8> {
        let mut dev = self.inner.lock();
        let dev_block = self.block_size;
        let mut buf = vec![0u8; FS_BLOCK_SIZE];
        let start_block = offset / dev_block;
        let mut current_block = start_block;
        let mut offset_in_block = offset % dev_block;
        let mut total_bytes_read = 0;

        while total_bytes_read < buf.len() {
            let bytes_to_copy = min(buf.len() - total_bytes_read, dev_block - offset_in_block);

            let mut block_data = vec![0u8; dev_block];
            dev.read_block(current_block as u64, &mut block_data)
                .expect("ext4_rs: read_block failed");

            buf[total_bytes_read..total_bytes_read + bytes_to_copy]
                .copy_from_slice(&block_data[offset_in_block..offset_in_block + bytes_to_copy]);

            total_bytes_read += bytes_to_copy;
            offset_in_block = 0;
            current_block += 1;
        }

        buf
    }

    fn write_offset(&self, offset: usize, data: &[u8]) {
        if data.is_empty() {
            return;
        }

        let mut dev = self.inner.lock();
        let dev_block = self.block_size;
        let bytes_to_write = data.len();
        let start_block = offset / dev_block;
        let mut current_block = start_block;
        let mut offset_in_block = offset % dev_block;
        let mut total_bytes_written = 0;

        while total_bytes_written < bytes_to_write {
            let bytes_to_copy = min(
                bytes_to_write - total_bytes_written,
                dev_block - offset_in_block,
            );

            let mut block_data = vec![0u8; dev_block];
            if bytes_to_copy != dev_block || offset_in_block != 0 {
                dev.read_block(current_block as u64, &mut block_data)
                    .expect("ext4_rs: read_block failed");
            }

            block_data[offset_in_block..offset_in_block + bytes_to_copy]
                .copy_from_slice(&data[total_bytes_written..total_bytes_written + bytes_to_copy]);

            dev.write_block(current_block as u64, &block_data)
                .expect("ext4_rs: write_block failed");

            total_bytes_written += bytes_to_copy;
            offset_in_block = 0;
            current_block += 1;
        }
    }
}

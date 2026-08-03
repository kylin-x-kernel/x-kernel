// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Block-device adaptation for filesystems.
//!
//! [`SeekableDisk`] adapts block I/O to byte-oriented filesystem libraries.
//! [`FileSystemType`] is the Kconfig-selected block filesystem mount contract;
//! the selected filesystem crate supplies its implementation at link time.
#![cfg_attr(any(not(test), doc), no_std)]

extern crate alloc;

use alloc::{boxed::Box, sync::Arc, vec};
use core::mem;

use kclass::{BlockDeviceImpl, ClassDevice, prelude::*};
use kvfs::{SuperBlock, SuperBlockFlags, VfsResult};

/// The block-backed filesystem type selected by Kconfig.
///
/// This is the mount capability of Linux `struct file_system_type`, expressed
/// as a Rust single-provider interface because the current Kconfig model
/// selects exactly one root filesystem implementation per kernel image.
#[kiface::interface]
pub trait FileSystemType {
    /// Mounts the selected filesystem from a block device.
    ///
    /// # Errors
    ///
    /// Returns a filesystem-specific error when the device cannot be mounted.
    fn mount_bdev(
        device: ClassDevice<BlockDeviceImpl>,
        flags: SuperBlockFlags,
    ) -> VfsResult<Arc<SuperBlock>>;
}

#[doc(hidden)]
pub use kiface;

/// Consume `cnt` bytes from the front of a slice.
fn take<'a>(buf: &mut &'a [u8], cnt: usize) -> &'a [u8] {
    let (first, rem) = buf.split_at(cnt);
    *buf = rem;
    first
}

/// Consume `cnt` bytes from the front of a mutable slice.
fn take_mut<'a>(buf: &mut &'a mut [u8], cnt: usize) -> &'a mut [u8] {
    // use mem::take to circumvent lifetime issues
    let (first, rem) = mem::take(buf).split_at_mut(cnt);
    *buf = rem;
    first
}

/// A block device with a byte cursor.
pub struct SeekableDisk {
    dev: ClassDevice<BlockDeviceImpl>,

    block_id: u64,
    offset: usize,
    block_size_log2: u8,

    read_buffer: Box<[u8]>,
    write_buffer: Box<[u8]>,
    /// Whether we have unsaved changes in the write buffer.
    ///
    /// It's guaranteed that when `offset == 0`, write_buffer_dirty is false.
    write_buffer_dirty: bool,
}

impl SeekableDisk {
    /// Creates a byte cursor over a block device.
    pub fn new(dev: ClassDevice<BlockDeviceImpl>) -> Self {
        assert!(dev.block_size().is_power_of_two());
        let block_size_log2 = dev.block_size().trailing_zeros() as u8;
        let read_buffer = vec![0u8; dev.block_size()].into_boxed_slice();
        let write_buffer = vec![0u8; dev.block_size()].into_boxed_slice();
        Self {
            dev,
            block_id: 0,
            offset: 0,
            block_size_log2,
            read_buffer,
            write_buffer,
            write_buffer_dirty: false,
        }
    }

    /// Returns the disk size in bytes.
    pub fn size(&self) -> u64 {
        self.dev.num_blocks() << self.block_size_log2
    }

    /// Returns the block size in bytes.
    pub fn block_size(&self) -> usize {
        1 << self.block_size_log2
    }

    /// Returns the current byte position.
    pub fn position(&self) -> u64 {
        (self.block_id << self.block_size_log2) + self.offset as u64
    }

    /// Sets the current byte position.
    pub fn set_position(&mut self, pos: u64) -> DriverResult<()> {
        self.flush()?;
        self.block_id = pos >> self.block_size_log2;
        self.offset = pos as usize & (self.block_size() - 1);
        Ok(())
    }

    /// Writes pending buffered data to the device.
    pub fn flush(&mut self) -> DriverResult<()> {
        if self.write_buffer_dirty {
            self.dev.write_block(self.block_id, &self.write_buffer)?;
            self.write_buffer_dirty = false;
        }
        self.dev.flush()?;
        Ok(())
    }

    fn read_partial(&mut self, buf: &mut &mut [u8]) -> DriverResult<usize> {
        self.flush()?;
        self.dev.read_block(self.block_id, &mut self.read_buffer)?;

        let data = &self.read_buffer[self.offset..];
        let length = buf.len().min(data.len());
        take_mut(buf, length).copy_from_slice(&data[..length]);

        self.offset += length;
        if self.offset == self.block_size() {
            self.block_id += 1;
            self.offset = 0;
        }

        Ok(length)
    }

    /// Reads bytes at the current cursor and advances it.
    pub fn read(&mut self, mut buf: &mut [u8]) -> DriverResult<usize> {
        let mut read = 0;
        if self.offset != 0 {
            read += self.read_partial(&mut buf)?;
        }
        if buf.len() >= self.block_size() {
            let blocks = buf.len() >> self.block_size_log2;
            let length = blocks << self.block_size_log2;
            self.dev
                .read_block(self.block_id, take_mut(&mut buf, length))?;
            read += length;

            self.block_id += blocks as u64;
        }
        if !buf.is_empty() {
            read += self.read_partial(&mut buf)?;
        }

        Ok(read)
    }

    fn write_partial(&mut self, buf: &mut &[u8]) -> DriverResult<usize> {
        if !self.write_buffer_dirty {
            self.dev.read_block(self.block_id, &mut self.write_buffer)?;
            self.write_buffer_dirty = true;
        }

        let data = &mut self.write_buffer[self.offset..];
        let length = buf.len().min(data.len());
        data[..length].copy_from_slice(take(buf, length));

        self.offset += length;
        if self.offset == self.block_size() {
            self.flush()?;
            self.block_id += 1;
            self.offset = 0;
        }

        Ok(length)
    }

    /// Writes bytes at the current cursor and advances it.
    pub fn write(&mut self, mut buf: &[u8]) -> DriverResult<usize> {
        let mut written = 0;
        if self.offset != 0 {
            written += self.write_partial(&mut buf)?;
        }
        if buf.len() >= self.block_size() {
            let blocks = buf.len() >> self.block_size_log2;
            let length = blocks << self.block_size_log2;
            self.dev
                .write_block(self.block_id, take(&mut buf, length))?;
            written += length;

            self.block_id += blocks as u64;
        }
        if !buf.is_empty() {
            written += self.write_partial(&mut buf)?;
        }

        Ok(written)
    }
}

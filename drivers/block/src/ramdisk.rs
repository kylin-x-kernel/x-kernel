// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! A RAM disk driver backed by heap-allocated memory.

extern crate alloc;

use alloc::alloc::{alloc_zeroed, dealloc};
use core::{
    alloc::Layout,
    ops::{Deref, DerefMut},
    ptr::NonNull,
};

use driver_base::{DeviceKind, DriverError, DriverOps, DriverResult};

use crate::BlockDriverOps;

const BLOCK_SIZE: usize = 512;

/// A RAM disk structure backed by heap memory.
pub struct RamDisk(NonNull<[u8]>);

unsafe impl Send for RamDisk {}
unsafe impl Sync for RamDisk {}

impl Default for RamDisk {
    fn default() -> Self {
        // Initially creates an empty dangling pointer for the RamDisk
        Self(NonNull::<[u8; 0]>::dangling())
    }
}

impl RamDisk {
    /// Creates a new RAM disk with the specified size hint.
    ///
    /// The size is rounded up to be aligned to the block size (512 bytes).
    pub fn new(size_hint: usize) -> Self {
        let size = align_up(size_hint);
        let layout = unsafe { Layout::from_size_align_unchecked(size, BLOCK_SIZE) };

        // Allocate the memory and create a NonNull pointer to the RAM disk buffer.
        let ptr = unsafe { NonNull::new_unchecked(alloc_zeroed(layout)) };

        Self(NonNull::slice_from_raw_parts(ptr, size))
    }
}

impl Drop for RamDisk {
    fn drop(&mut self) {
        if self.0.is_empty() {
            return;
        }

        // Deallocate the memory when the RamDisk goes out of scope
        unsafe {
            dealloc(
                self.0.cast::<u8>().as_ptr(),
                Layout::from_size_align_unchecked(self.0.len(), BLOCK_SIZE),
            );
        }
    }
}

impl Deref for RamDisk {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        // Dereferencing the RamDisk to get a slice of bytes
        unsafe { self.0.as_ref() }
    }
}

impl DerefMut for RamDisk {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // Dereferencing mutably for mutable operations
        unsafe { self.0.as_mut() }
    }
}

impl From<&[u8]> for RamDisk {
    fn from(data: &[u8]) -> Self {
        let mut ramdisk = RamDisk::new(data.len());
        ramdisk[..data.len()].copy_from_slice(data);
        ramdisk
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
    #[inline]
    fn num_blocks(&self) -> u64 {
        // Calculates the number of blocks in the RAM disk
        (self.len() / BLOCK_SIZE) as u64
    }

    #[inline]
    fn block_size(&self) -> usize {
        BLOCK_SIZE
    }

    fn read_block(&mut self, block_id: u64, buf: &mut [u8]) -> DriverResult {
        if buf.len() % BLOCK_SIZE != 0 {
            return Err(DriverError::InvalidInput);
        }
        let offset = block_id as usize * BLOCK_SIZE;
        if offset + buf.len() > self.len() {
            return Err(DriverError::Io);
        }
        buf.copy_from_slice(&self[offset..offset + buf.len()]);
        Ok(())
    }

    fn write_block(&mut self, block_id: u64, buf: &[u8]) -> DriverResult {
        if buf.len() % BLOCK_SIZE != 0 {
            return Err(DriverError::InvalidInput);
        }
        let offset = block_id as usize * BLOCK_SIZE;
        if offset + buf.len() > self.len() {
            return Err(DriverError::Io);
        }
        self[offset..offset + buf.len()].copy_from_slice(buf);
        Ok(())
    }

    fn flush(&mut self) -> DriverResult {
        Ok(())
    }
}

/// Aligns a given size upwards to the nearest multiple of `BLOCK_SIZE`.
const fn align_up(val: usize) -> usize {
    (val + BLOCK_SIZE - 1) & !(BLOCK_SIZE - 1)
}

#[cfg(unittest)]
mod tests_ramdisk {
    use unittest::{assert, assert_eq, def_test};

    use super::*;
    extern crate alloc;
    use alloc::vec;

    #[def_test]
    fn test_ramdisk_basic_operations() {
        // Test creation with different sizes
        let mut disk = RamDisk::new(1024);
        assert_eq!(disk.device_kind(), DeviceKind::Block);
        assert_eq!(disk.name(), "ramdisk");
        assert_eq!(disk.block_size(), 512);
        assert_eq!(disk.num_blocks(), 2);

        // Test basic read/write operations
        let test_data = vec![0xAA; 512];
        let mut read_buf = vec![0; 512];

        // Write to first block
        assert!(disk.write_block(0, &test_data).is_ok());

        // Read back and verify
        assert!(disk.read_block(0, &mut read_buf).is_ok());
        assert_eq!(read_buf, test_data);

        // Test second block with different pattern
        let test_data2 = vec![0x55; 512];
        assert!(disk.write_block(1, &test_data2).is_ok());
        assert!(disk.read_block(1, &mut read_buf).is_ok());
        assert_eq!(read_buf, test_data2);

        // Verify first block is unchanged
        assert!(disk.read_block(0, &mut read_buf).is_ok());
        assert_eq!(read_buf, test_data);

        // Test flush operation
        assert!(disk.flush().is_ok());
    }

    #[def_test]
    fn test_ramdisk_boundary_conditions() {
        let mut disk = RamDisk::new(1536); // 3 blocks

        // Test reading beyond disk boundary
        let mut buf = vec![0; 512];
        assert_eq!(disk.read_block(3, &mut buf), Err(DriverError::Io));
        assert_eq!(disk.read_block(100, &mut buf), Err(DriverError::Io));

        // Test writing beyond disk boundary
        let data = vec![0xFF; 512];
        assert_eq!(disk.write_block(3, &data), Err(DriverError::Io));
        assert_eq!(disk.write_block(100, &data), Err(DriverError::Io));

        // Test invalid buffer sizes (not multiple of block size)
        let mut invalid_buf = vec![0; 510]; // Not aligned to block size
        assert_eq!(
            disk.read_block(0, &mut invalid_buf),
            Err(DriverError::InvalidInput)
        );

        let invalid_data = vec![0xFF; 510];
        assert_eq!(
            disk.write_block(0, &invalid_data),
            Err(DriverError::InvalidInput)
        );

        // Test multi-block operations at boundaries
        let mut multi_buf = vec![0; 1024]; // 2 blocks

        // This should work (blocks 0-1)
        assert!(disk.read_block(0, &mut multi_buf).is_ok());
        assert!(disk.write_block(0, &multi_buf).is_ok());

        // This should work (blocks 1-2)
        assert!(disk.read_block(1, &mut multi_buf).is_ok());
        assert!(disk.write_block(1, &multi_buf).is_ok());

        // This should fail (blocks 2-3, but only block 2 exists)
        assert_eq!(disk.read_block(2, &mut multi_buf), Err(DriverError::Io));
        assert_eq!(disk.write_block(2, &multi_buf), Err(DriverError::Io));

        // Test edge case: exactly at boundary
        let edge_data = vec![0xCC; 512];
        assert!(disk.write_block(2, &edge_data).is_ok()); // Last valid block

        let mut edge_buf = vec![0; 512];
        assert!(disk.read_block(2, &mut edge_buf).is_ok());
        assert_eq!(edge_buf, edge_data);

        // Test creation from slice
        let source_data = vec![0x12; 2048];
        let disk_from_slice = RamDisk::from(source_data.as_slice());
        assert_eq!(disk_from_slice.num_blocks(), 4);

        let mut verify_buf = vec![0; 512];
        let disk_from_slice_mut = &mut RamDisk::from(source_data.as_slice());
        assert!(disk_from_slice_mut.read_block(0, &mut verify_buf).is_ok());
        assert_eq!(verify_buf, vec![0x12; 512]);
    }
}

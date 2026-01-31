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

#[cfg(unittest)]
pub mod tests_ramdisk_static {
    use unittest::def_test;

    use super::*;
    extern crate alloc;
    use alloc::{
        alloc::{Layout, alloc},
        vec,
    };

    // Helper function to create aligned static buffer for testing
    fn create_aligned_buffer(size: usize) -> &'static mut [u8] {
        let layout = Layout::from_size_align(size, 512).unwrap();
        let ptr = unsafe { alloc(layout) };
        if ptr.is_null() {
            panic!("Failed to allocate memory");
        }

        unsafe {
            ptr.write_bytes(0, size);
            core::slice::from_raw_parts_mut(ptr, size)
        }
    }

    #[def_test]
    fn test_ramdisk_static_basic_operations() {
        let buffer = create_aligned_buffer(2048); // 4 blocks
        let mut disk = RamDisk::new(buffer);

        // Test driver properties
        assert_eq!(disk.device_kind(), DeviceKind::Block);
        assert_eq!(disk.name(), "ramdisk");
        assert_eq!(disk.block_size(), 512);
        assert_eq!(disk.num_blocks(), 4);

        // Test basic read/write operations
        let test_pattern = vec![0xAB; 512];
        let mut read_buffer = vec![0; 512];

        // Write and verify first block
        assert!(disk.write_block(0, &test_pattern).is_ok());
        assert!(disk.read_block(0, &mut read_buffer).is_ok());
        assert_eq!(read_buffer, test_pattern);

        // Test different patterns on different blocks
        let patterns = [
            vec![0x11; 512],
            vec![0x22; 512],
            vec![0x33; 512],
            vec![0x44; 512],
        ];

        // Write different patterns to each block
        for (i, pattern) in patterns.iter().enumerate() {
            assert!(disk.write_block(i as u64, pattern).is_ok());
        }

        // Verify each pattern is preserved
        for (i, expected_pattern) in patterns.iter().enumerate() {
            let mut buffer = vec![0; 512];
            assert!(disk.read_block(i as u64, &mut buffer).is_ok());
            assert_eq!(&buffer, expected_pattern);
        }

        // Test flush operation
        assert!(disk.flush().is_ok());
    }

    #[def_test]
    fn test_ramdisk_static_boundary_and_errors() {
        let buffer = create_aligned_buffer(1024); // 2 blocks
        let mut disk = RamDisk::new(buffer);

        // Test reading/writing beyond boundaries
        let mut test_buf = vec![0; 512];
        let test_data = vec![0xFF; 512];

        // Valid operations
        assert!(disk.read_block(0, &mut test_buf).is_ok());
        assert!(disk.read_block(1, &mut test_buf).is_ok());
        assert!(disk.write_block(0, &test_data).is_ok());
        assert!(disk.write_block(1, &test_data).is_ok());

        // Invalid operations - beyond disk boundary
        assert_eq!(disk.read_block(2, &mut test_buf), Err(DriverError::Io));
        assert_eq!(disk.read_block(100, &mut test_buf), Err(DriverError::Io));
        assert_eq!(disk.write_block(2, &test_data), Err(DriverError::Io));
        assert_eq!(disk.write_block(100, &test_data), Err(DriverError::Io));

        // Test buffer size validation - must be multiple of block size
        let mut invalid_buf = vec![0; 511]; // One byte short
        assert_eq!(
            disk.read_block(0, &mut invalid_buf),
            Err(DriverError::InvalidInput)
        );

        let invalid_data = vec![0xAA; 513]; // One byte over
        assert_eq!(
            disk.write_block(0, &invalid_data),
            Err(DriverError::InvalidInput)
        );

        // Test multi-block operations
        let mut multi_buf = vec![0; 1024]; // Exactly 2 blocks
        let multi_data = vec![0xCC; 1024];

        // Valid multi-block operations
        assert!(disk.write_block(0, &multi_data).is_ok());
        assert!(disk.read_block(0, &mut multi_buf).is_ok());
        assert_eq!(multi_buf, multi_data);

        // Invalid multi-block operation - extends beyond disk
        let mut oversized_buf = vec![0; 1536]; // 3 blocks, but disk only has 2
        assert_eq!(disk.read_block(0, &mut oversized_buf), Err(DriverError::Io));

        let oversized_data = vec![0xDD; 1536];
        assert_eq!(disk.write_block(0, &oversized_data), Err(DriverError::Io));

        // Test edge case: read/write starting from block 1 with 2-block buffer
        let mut edge_buf = vec![0; 1024]; // 2 blocks
        assert_eq!(disk.read_block(1, &mut edge_buf), Err(DriverError::Io)); // Would read blocks 1-2, but block 2 doesn't exist
    }
}

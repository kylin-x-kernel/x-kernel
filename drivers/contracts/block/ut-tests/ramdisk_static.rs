// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;
extern crate alloc;
use alloc::{
    alloc::{Layout, alloc},
    vec,
};

// Helper function to create aligned static buffer for testing
fn create_aligned_buffer(size: usize) -> &'static mut [u8] {
    assert!(size > 0, "test buffer allocation must be nonempty");
    let layout = Layout::from_size_align(size, 512).unwrap();
    // SAFETY: the assertion above guarantees a nonzero allocation size;
    // `Layout::from_size_align` validates alignment and size bounds. The
    // returned pointer is only used after the null check below.
    let ptr = unsafe { alloc(layout) };
    if ptr.is_null() {
        panic!("Failed to allocate memory");
    }

    // SAFETY: the checked non-null allocation owns `size` writable bytes
    // and has not been aliased; zeroing this range initializes every byte.
    unsafe { ptr.write_bytes(0, size) };
    // SAFETY: the initialized allocation is aligned, exclusively owned and
    // intentionally leaked. The slice spans exactly its `size` bytes.
    unsafe { core::slice::from_raw_parts_mut(ptr, size) }
}

#[def_test]
fn test_ramdisk_static_basic_operations() {
    let buffer = create_aligned_buffer(2048); // 4 blocks
    let disk = RamDisk::new(buffer);

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
    let disk = RamDisk::new(buffer);

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

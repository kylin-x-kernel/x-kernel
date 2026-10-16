// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, def_test};

use super::*;
extern crate alloc;
use alloc::vec;

#[def_test]
fn test_ramdisk_basic_operations() {
    // Test creation with different sizes
    let disk = RamDisk::new(1024);
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
fn test_ramdisk_num_blocks_matches_capacity() {
    let disk = RamDisk::from(&[0x5Au8; 700][..]);
    assert_eq!(disk.num_blocks(), 2);
    assert!(disk.write_block(1, &[0xA5; 512]).is_ok());
    assert_eq!(disk.num_blocks(), 2);
}

#[def_test]
fn test_ramdisk_boundary_conditions() {
    let disk = RamDisk::new(1536); // 3 blocks

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

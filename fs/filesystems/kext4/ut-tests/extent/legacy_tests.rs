// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Legacy mapping boundaries on sparse synthetic metadata, without mkfs.

use alloc::{sync::Arc, vec, vec::Vec};

use block::{BlockDeviceOperations, Device, DeviceKind, DriverError, DriverResult};
use unittest::def_test;

use crate::{
    BlockMapping, CorruptKind, Ext4Error, Ext4SbInfo, InodeNumber, LogicalBlock,
    superblock::{
        Ext4StatFsMode,
        test_fixtures::{allocator_superblock_with_geometry, put_u16, put_u32},
    },
};

// A 512-byte read-only device supports both filesystem block sizes without
// adding configurable geometry or write/fault state to the shared fixture.
struct ReadOnlyDevice(Vec<u8>);

impl Device for ReadOnlyDevice {
    fn name(&self) -> &str {
        "legacy-mapping-test"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Block
    }
}

impl BlockDeviceOperations for ReadOnlyDevice {
    fn num_blocks(&self) -> u64 {
        (self.0.len() / 512) as u64
    }

    fn block_size(&self) -> usize {
        512
    }

    fn read_block(&self, block: u64, output: &mut [u8]) -> DriverResult {
        let start = usize::try_from(block)
            .ok()
            .and_then(|block| block.checked_mul(512))
            .ok_or(DriverError::InvalidInput)?;
        let end = start
            .checked_add(output.len())
            .ok_or(DriverError::InvalidInput)?;
        output.copy_from_slice(self.0.get(start..end).ok_or(DriverError::InvalidInput)?);
        Ok(())
    }

    fn write_block(&self, _: u64, _: &[u8]) -> DriverResult {
        Err(DriverError::Unsupported)
    }

    fn flush(&self) -> DriverResult {
        Ok(())
    }
}

fn samples(block_size: usize) -> [u64; 8] {
    let pointers = (block_size / 4) as u64;
    [
        0,
        11,
        12,
        11 + pointers,
        12 + pointers,
        12 + 2 * pointers,
        11 + pointers + pointers * pointers,
        12 + pointers + pointers * pointers,
    ]
}

fn image(block_size: usize) -> Vec<u8> {
    let mut bytes = vec![0; 64 * block_size];
    let first_data = u32::from(block_size == 1024);
    let mut sb = allocator_superblock_with_geometry(64, 32, 0, 0);
    put_u32(&mut sb, 0x14, first_data);
    let shift = (block_size / 1024).trailing_zeros();
    put_u32(&mut sb, 0x18, shift);
    put_u32(&mut sb, 0x1c, shift);
    put_u32(&mut sb, 0x20, 64);
    put_u32(&mut sb, 0x24, 64);
    bytes[1024..2048].copy_from_slice(&sb);
    let descriptor = &mut bytes[(first_data as usize + 1) * block_size..];
    put_u32(descriptor, 0, 3);
    put_u32(descriptor, 4, 4);
    put_u32(descriptor, 8, 5);
    bytes[3 * block_size..5 * block_size].fill(0xff);

    let inode = &mut bytes[5 * block_size + 10 * 256..][..256];
    put_u16(inode, 0, 0x81a4);
    put_u16(inode, 0x1a, 1);
    let size = (samples(block_size)[7] + 1) * block_size as u64;
    put_u32(inode, 4, size as u32);
    put_u32(inode, 0x6c, (size >> 32) as u32);
    put_u32(inode, 0x1c, (16 * block_size / 512) as u32);
    for (index, physical) in [(0, 24), (11, 25), (12, 14), (13, 15), (14, 19)] {
        put_u32(inode, 0x28 + index * 4, physical);
    }
    let last = block_size / 4 - 1;
    for (block, index, pointer) in [
        (14, 0, 26),
        (14, last, 27),
        (15, 0, 16),
        (15, 1, 17),
        (15, last, 18),
        (16, 0, 28),
        (17, 0, 29),
        (18, last, 30),
        (19, 0, 20),
        (20, 0, 21),
        (21, 0, 31),
    ] {
        put_u32(&mut bytes, block * block_size + index * 4, pointer);
    }
    for (index, logical) in samples(block_size).into_iter().enumerate() {
        bytes[(24 + index) * block_size..(25 + index) * block_size]
            .fill(logical.wrapping_mul(37).wrapping_add(1) as u8);
    }
    bytes
}

#[def_test]
fn legacy_mapping_reads_all_indirection_boundaries_and_rejects_bad_pointers() {
    for block_size in [1024, 4096] {
        let bytes = image(block_size);
        // The filesystem supports extents; this particular inode uses legacy
        // pointers. Do not bypass the mount rejection of non-extent filesystems.
        let filesystem = Ext4SbInfo::prepare_mount_with_statfs_mode(
            Arc::new(ReadOnlyDevice(bytes.clone())),
            Ext4StatFsMode::Bsd,
        )
        .unwrap();
        let inode = filesystem.load_inode_private(InodeNumber::new(11)).unwrap();
        assert!(!inode.has_extents());
        for (index, logical) in samples(block_size).into_iter().enumerate() {
            assert!(
                matches!(filesystem.map_blocks(&inode, LogicalBlock::new(logical)),
                Ok(BlockMapping::Mapped { physical, .. }) if physical.get() == 24 + index as u64)
            );
            let mut actual = vec![0; block_size];
            assert_eq!(
                filesystem
                    .read_at(&inode, logical * block_size as u64, &mut actual)
                    .unwrap(),
                block_size
            );
            assert!(
                actual
                    .iter()
                    .all(|byte| *byte == logical.wrapping_mul(37).wrapping_add(1) as u8)
            );
        }
        assert!(matches!(
            filesystem.map_blocks(&inode, LogicalBlock::new(samples(block_size)[7] + 1)),
            Ok(BlockMapping::Hole { .. })
        ));
        for invalid in [1, 80] {
            let mut corrupt = bytes.clone();
            put_u32(&mut corrupt, 14 * block_size, invalid);
            let filesystem = Ext4SbInfo::prepare_mount_with_statfs_mode(
                Arc::new(ReadOnlyDevice(corrupt)),
                Ext4StatFsMode::Bsd,
            )
            .unwrap();
            let inode = filesystem.load_inode_private(InodeNumber::new(11)).unwrap();
            assert_eq!(
                filesystem.map_blocks(&inode, LogicalBlock::new(12)),
                Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent))
            );
        }
    }
}

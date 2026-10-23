// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! File-read destination, geometry, boundary and failure contracts.

use alloc::{sync::Arc, vec, vec::Vec};

use block::{BlockDeviceOperations, Device, DeviceKind, DriverError, DriverResult};
use unittest::def_test;

use crate::{
    Ext4Error, Ext4Inode, Ext4SbInfo, InodeNumber,
    superblock::test_fixtures::{
        allocator_group_descriptor, allocator_superblock, put_u16, put_u32,
    },
    sync::Mutex,
};

pub(crate) struct ReadDevice {
    inner: Arc<dyn BlockDeviceOperations>,
    // The public trait call and the restricted transfer may use different
    // addresses. Record both without changing the existing raw-I/O assertions.
    caller_destinations: Mutex<Vec<usize>>,
    pub(crate) destinations: Mutex<Vec<(u64, usize, usize)>>,
    pub(crate) fail_reads: Mutex<bool>,
}

impl ReadDevice {
    pub(crate) fn new(inner: Arc<dyn BlockDeviceOperations>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            caller_destinations: Mutex::new(Vec::new()),
            destinations: Mutex::new(Vec::new()),
            fail_reads: Mutex::new(false),
        })
    }
}

struct ReadStorage {
    bytes: Mutex<Vec<u8>>,
    block_size: usize,
}

impl Device for ReadDevice {
    fn name(&self) -> &str {
        "kext4-read-destination-test"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Block
    }
}

impl BlockDeviceOperations for ReadDevice {
    fn num_blocks(&self) -> u64 {
        self.inner.num_blocks()
    }

    fn block_size(&self) -> usize {
        self.inner.block_size()
    }

    fn read_block(&self, block: u64, output: &mut [u8]) -> DriverResult {
        self.caller_destinations
            .lock()
            .push(output.as_ptr() as usize);
        block::word_io::read_blocks_with_provider(
            block::word_io::test_provider(),
            block,
            self.block_size(),
            self.num_blocks(),
            output,
            |block, aligned| {
                self.destinations
                    .lock()
                    .push((block, aligned.as_ptr() as usize, aligned.len()));
                // Keep the restricted raw backend, including before EIO.
                if !(aligned.as_ptr() as usize).is_multiple_of(4) {
                    return Err(DriverError::InvalidInput);
                }
                if *self.fail_reads.lock() {
                    // Failed I/O may have modified a prefix. Leave failure
                    // enabled until the test starts a successful full retry.
                    let prefix = aligned.len().min(17);
                    aligned[..prefix].fill(0xee);
                    return Err(DriverError::Io);
                }
                self.inner.read_block(block, aligned)
            },
        )
    }

    fn write_block(&self, block: u64, input: &[u8]) -> DriverResult {
        self.inner.write_block(block, input)
    }

    fn flush(&self) -> DriverResult {
        self.inner.flush()
    }
}

impl Device for ReadStorage {
    fn name(&self) -> &str {
        "kext4-read-storage-test"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Block
    }
}

impl BlockDeviceOperations for ReadStorage {
    fn num_blocks(&self) -> u64 {
        (self.bytes.lock().len() / self.block_size) as u64
    }

    fn block_size(&self) -> usize {
        self.block_size
    }

    fn read_block(&self, block: u64, output: &mut [u8]) -> DriverResult {
        let start = usize::try_from(block)
            .ok()
            .and_then(|block| block.checked_mul(self.block_size))
            .ok_or(DriverError::InvalidInput)?;
        let end = start
            .checked_add(output.len())
            .ok_or(DriverError::InvalidInput)?;
        output.copy_from_slice(
            self.bytes
                .lock()
                .get(start..end)
                .ok_or(DriverError::InvalidInput)?,
        );
        Ok(())
    }

    fn write_block(&self, block: u64, input: &[u8]) -> DriverResult {
        let start = usize::try_from(block)
            .ok()
            .and_then(|block| block.checked_mul(self.block_size))
            .ok_or(DriverError::InvalidInput)?;
        let end = start
            .checked_add(input.len())
            .ok_or(DriverError::InvalidInput)?;
        self.bytes
            .lock()
            .get_mut(start..end)
            .ok_or(DriverError::InvalidInput)?
            .copy_from_slice(input);
        Ok(())
    }

    fn flush(&self) -> DriverResult {
        Ok(())
    }
}

pub(crate) fn read_fixture(
    block_size: usize,
    device_block_size: usize,
) -> (Ext4SbInfo, Ext4Inode, Arc<ReadDevice>, Vec<u8>) {
    read_fixture_with_blocks(block_size, device_block_size, 32)
}

#[expect(
    clippy::unwrap_used,
    reason = "CG-021-no-panic-on-reachable-failure: test scenario: malformed synthetic metadata \
              must abort fixture setup, not mask a read-contract regression"
)]
pub(crate) fn read_fixture_with_blocks(
    block_size: usize,
    device_block_size: usize,
    block_count: usize,
) -> (Ext4SbInfo, Ext4Inode, Arc<ReadDevice>, Vec<u8>) {
    // One group, 32 inodes, three inline extents. Keep the small image
    // read-only: allocation, root-directory and journal fixtures are unrelated.
    let first_data_block = usize::from(block_size == 1024);
    let descriptor_block = first_data_block + 1;
    let inode_table_block = descriptor_block + 3;
    let mut image = vec![0; block_count * block_size];
    let mut superblock = allocator_superblock(0, 0);
    put_u32(&mut superblock, 0x04, block_count as u32);
    put_u32(&mut superblock, 0x20, block_count as u32);
    put_u32(&mut superblock, 0x24, block_count as u32);
    put_u32(&mut superblock, 0x14, first_data_block as u32);
    let log_block_size = (block_size / 1024).trailing_zeros();
    put_u32(&mut superblock, 0x18, log_block_size);
    put_u32(&mut superblock, 0x1c, log_block_size);
    image[1024..2048].copy_from_slice(&superblock);
    let mut descriptor = allocator_group_descriptor(0, 0, 0, 0);
    for (offset, block) in [
        (0, descriptor_block + 1),
        (4, descriptor_block + 2),
        (8, inode_table_block),
    ] {
        put_u32(&mut descriptor, offset, block as u32);
    }
    let descriptor_offset = descriptor_block * block_size;
    image[descriptor_offset..descriptor_offset + descriptor.len()].copy_from_slice(&descriptor);

    let file_len = 6 * block_size + 17;
    let inode_offset = inode_table_block * block_size + 11 * 256;
    let inode = &mut image[inode_offset..inode_offset + 256];
    put_u16(inode, 0, 0o100644);
    put_u32(inode, 4, file_len as u32);
    put_u16(inode, 0x1a, 1);
    put_u32(inode, 0x1c, (6 * block_size / 512) as u32);
    put_u32(inode, 0x20, crate::disk::inode::EXT4_EXTENTS_FL);
    put_u16(inode, 0x28, 0xf30a);
    put_u16(inode, 0x2a, 3);
    put_u16(inode, 0x2c, 4);
    for (slot, logical, len, physical) in [(0, 0, 3, 20), (1, 4, 0x8002, 23), (2, 6, 1, 25)] {
        let entry = 0x34 + slot * 12;
        put_u32(inode, entry, logical);
        put_u16(inode, entry + 4, len);
        put_u32(inode, entry + 8, physical);
    }
    let mut expected = vec![0; file_len];
    for (index, byte) in expected[..3 * block_size].iter_mut().enumerate() {
        *byte = (index % 251) as u8;
    }
    expected[6 * block_size..].fill(0x7a);
    image[20 * block_size..23 * block_size].copy_from_slice(&expected[..3 * block_size]);
    image[23 * block_size..25 * block_size].fill(0xee); // Unwritten data must not escape.
    image[25 * block_size..26 * block_size].fill(0x7a); // Real partial EOF block.

    let device = ReadDevice::new(Arc::new(ReadStorage {
        bytes: Mutex::new(image),
        block_size: device_block_size,
    }));
    let filesystem = Ext4SbInfo::open(device.clone(), false).unwrap();
    let inode = filesystem.load_inode_private(InodeNumber::new(12)).unwrap();
    device.destinations.lock().clear();
    (filesystem, inode, device, expected)
}

#[def_test]
fn read_backend_adapter_accepts_unaligned_buffers_and_preserves_failed_chunk() {
    let (_, _, device, expected) = read_fixture(4096, 512);
    let mut storage = vec![0xa5; 520];
    let start = storage.as_ptr().align_offset(4) + 1;
    let output = &mut storage[start..start + 512];
    let caller = output.as_ptr() as usize;
    device.read_block(20 * 4096 / 512, output).unwrap();
    assert_eq!(output, &expected[..512]);
    assert_eq!(device.caller_destinations.lock().last(), Some(&caller));
    assert_ne!(device.destinations.lock().last().unwrap().1, caller);
    output.fill(0xa5);
    *device.fail_reads.lock() = true;
    assert_eq!(
        device.read_block(20 * 4096 / 512, output),
        Err(DriverError::Io)
    );
    assert!(output.iter().all(|&byte| byte == 0xa5));
    *device.fail_reads.lock() = false;
    device.read_block(20 * 4096 / 512, output).unwrap();
    assert_eq!(output, &expected[..512]);
}

#[def_test]
fn file_reads_preserve_block_boundaries_holes_unwritten_and_eof() {
    for (block_size, device_block_size) in [(1024, 512), (2048, 512), (4096, 512), (4096, 4096)] {
        let (filesystem, inode, device, expected) = read_fixture(block_size, device_block_size);
        let file_len = expected.len();
        for ((offset, requested), shift) in [
            (0, 3 * block_size),
            (block_size, 2 * block_size),
            (1, 3 * block_size),
            (2 * block_size - 17, 4 * block_size + 50),
            (3 * block_size, 3 * block_size), // Hole plus unwritten run: no data I/O.
            (6 * block_size, 20),
            (file_len - 9, 20),
            (file_len, 20),
            (0, 0),
            (0, 7), // A destination shorter than one filesystem block.
            (0, file_len + 33),
        ]
        .into_iter()
        .flat_map(|range| [0, 3].map(|shift| (range, shift)))
        {
            let mut output = vec![0xa5; requested + 16];
            let start = output.as_ptr().align_offset(4) + 4 + shift;
            let destination = output.as_ptr() as usize + start;
            let copied = filesystem
                .read_at(&inode, offset as u64, &mut output[start..start + requested])
                .unwrap();
            assert_eq!(copied, requested.min(file_len - offset));
            assert_eq!(
                &output[start..start + copied],
                &expected[offset..offset + copied]
            );
            assert!(output[..start].iter().all(|&byte| byte == 0xa5));
            assert!(output[start + copied..].iter().all(|&byte| byte == 0xa5));
            let reads = core::mem::take(&mut *device.destinations.lock());
            if requested == 0 || offset == file_len || offset == 3 * block_size {
                assert!(
                    reads.is_empty(),
                    "empty ranges and sparse runs must not issue data I/O"
                );
            } else if shift == 0 && (offset == 0 && requested >= block_size || offset == block_size)
            {
                let bytes = (3 * block_size - offset).min(requested);
                assert_eq!(
                    reads[0],
                    (
                        (20 * block_size + offset) as u64 / device_block_size as u64,
                        destination,
                        bytes
                    )
                );
            } else {
                assert_ne!(
                    reads[0].1, destination,
                    "partial blocks and unaligned destinations require staging"
                );
            }
        }
    }
}

#[def_test]
fn unaligned_whole_blocks_reach_backend_without_filesystem_staging() {
    let (filesystem, inode, device, expected) = read_fixture(4096, 512);
    device.caller_destinations.lock().clear();
    device.destinations.lock().clear();
    let mut storage = vec![0xa5; 8192 + 8];
    let start = storage.as_ptr().align_offset(4) + 3;
    let output = &mut storage[start..start + 8192];
    let caller = output.as_ptr() as usize;
    assert_eq!(filesystem.read_at(&inode, 4096, output).unwrap(), 8192);
    assert_eq!(output, &expected[4096..3 * 4096]);
    // Ext4 hands its caller's exact address to the block trait. Only the
    // restricted raw transfer receives a separately owned, aligned buffer.
    assert_eq!(&*device.caller_destinations.lock(), &[caller]);
    let reads = device.destinations.lock();
    assert_eq!(reads.len(), 1);
    assert_eq!(reads[0].0, 21 * 4096 / 512);
    assert_eq!(reads[0].2, 8192);
    assert_ne!(reads[0].1, caller);
    assert_eq!(reads[0].1 % 4, 0);
    drop(reads);

    output.fill(0xa5);
    *device.fail_reads.lock() = true;
    assert_eq!(
        filesystem.read_at(&inode, 4096, output),
        Err(Ext4Error::Device(DriverError::Io))
    );
    assert!(output.iter().all(|&byte| byte == 0xa5));
    *device.fail_reads.lock() = false;
    assert_eq!(filesystem.read_at(&inode, 4096, output).unwrap(), 8192);
    assert_eq!(output, &expected[4096..3 * 4096]);
    assert!(storage[..start].iter().all(|&byte| byte == 0xa5));
    assert!(storage[start + 8192..].iter().all(|&byte| byte == 0xa5));
}

#[def_test]
fn aligned_file_read_reports_io_failure_and_retry_reinitializes_output() {
    let (filesystem, inode, device, expected) = read_fixture(4096, 512);
    let mut output = vec![0xa5; 4096 + 16];
    let start = output.as_ptr().align_offset(4) + 4;
    *device.fail_reads.lock() = true;
    assert_eq!(
        filesystem.read_at(&inode, 0, &mut output[start..start + 4096]),
        Err(Ext4Error::Device(DriverError::Io))
    );
    assert_eq!(&output[start..start + 17], &[0xee; 17]);
    assert!(output[..start].iter().all(|&byte| byte == 0xa5));
    assert!(output[start + 4096..].iter().all(|&byte| byte == 0xa5));
    // Failed contents are not a completed read; retry must overwrite all bytes.
    *device.fail_reads.lock() = false;
    assert_eq!(
        filesystem.read_at(&inode, 0, &mut output[start..start + 4096]),
        Ok(4096)
    );
    assert_eq!(&output[start..start + 4096], &expected[..4096]);
    assert!(output[..start].iter().all(|&byte| byte == 0xa5));
    assert!(output[start + 4096..].iter().all(|&byte| byte == 0xa5));
}

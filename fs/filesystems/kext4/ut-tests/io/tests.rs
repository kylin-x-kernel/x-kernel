// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec::Vec;

use block::{Device, DeviceKind, DriverError, DriverResult};
use unittest::def_test;

use super::*;
use crate::sync::Mutex;

const DEVICE_BLOCK_SIZE: usize = 512;

struct MemoryBlockDevice {
    bytes: Mutex<Vec<u8>>,
    write_lengths: Mutex<Vec<usize>>,
    is_flushed: Mutex<bool>,
}

impl MemoryBlockDevice {
    fn new(device_blocks: usize) -> Self {
        Self {
            bytes: Mutex::new(vec![0; device_blocks * DEVICE_BLOCK_SIZE]),
            write_lengths: Mutex::new(Vec::new()),
            is_flushed: Mutex::new(false),
        }
    }
}

impl Device for MemoryBlockDevice {
    fn name(&self) -> &str {
        "kext4-memory-block-device"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Block
    }
}

impl BlockDeviceOperations for MemoryBlockDevice {
    fn num_blocks(&self) -> u64 {
        (self.bytes.lock().len() / DEVICE_BLOCK_SIZE) as u64
    }

    fn block_size(&self) -> usize {
        DEVICE_BLOCK_SIZE
    }

    fn read_block(&self, block_id: u64, output: &mut [u8]) -> DriverResult {
        let start = usize::try_from(block_id)
            .map_err(|_| DriverError::InvalidInput)?
            .checked_mul(DEVICE_BLOCK_SIZE)
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

    fn write_block(&self, block_id: u64, input: &[u8]) -> DriverResult {
        self.write_lengths.lock().push(input.len());
        let start = usize::try_from(block_id)
            .map_err(|_| DriverError::InvalidInput)?
            .checked_mul(DEVICE_BLOCK_SIZE)
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
        *self.is_flushed.lock() = true;
        Ok(())
    }
}

#[def_test]
fn writes_filesystem_blocks_through_device_block_mapping() {
    let device = Arc::new(MemoryBlockDevice::new(8));
    let filesystem = FilesystemDevice::open(device.clone(), 1024, 4).unwrap();
    let input = vec![0x5a; 1024];

    filesystem
        .write_contiguous_blocks(FilesystemBlock::new(2), 1, &input)
        .unwrap();

    let bytes = device.bytes.lock();
    assert_eq!(&bytes[2048..3072], input.as_slice());
    assert_eq!(device.write_lengths.lock().as_slice(), &[1024]);
}

#[def_test]
fn rejects_partial_contiguous_filesystem_block_write() {
    let device = Arc::new(MemoryBlockDevice::new(8));
    let filesystem = FilesystemDevice::open(device, 1024, 4).unwrap();

    assert_eq!(
        filesystem.write_contiguous_blocks(FilesystemBlock::new(1), 1, &[0; 512]),
        Err(Ext4Error::InvalidBufferLength {
            expected: 1024,
            actual: 512,
        })
    );
}

#[def_test]
fn replay_writer_writes_one_block_and_flushes() {
    let device = Arc::new(MemoryBlockDevice::new(8));
    let filesystem = FilesystemDevice::open(device.clone(), 1024, 4).unwrap();
    let input = vec![0xa5; 1024];

    filesystem
        .write_replay_block(JournalTargetBlock::new(1), &input)
        .unwrap();
    filesystem.flush_replay().unwrap();

    let bytes = device.bytes.lock();
    assert_eq!(&bytes[1024..2048], input.as_slice());
    assert!(*device.is_flushed.lock());
}
